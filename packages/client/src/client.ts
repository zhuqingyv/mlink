import { EventEmitter } from "events";
import {
  ClientEvents,
  ClientOptions,
  Message,
  MlinkError,
  Peer,
  RawInFrame,
  ReadyInfo,
  RoomState,
  WS_PROTOCOL_VERSION,
} from "./types";

// Minimal WebSocket surface we rely on. Both Node's `ws` and the browser's
// native WebSocket implement it (modulo the `binaryType` knob, which we do
// not use — mlink WS is text-only).
interface WSLike {
  readyState: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  // Assignable handlers work for both browser WebSocket and `ws`.
  onopen: ((ev: unknown) => void) | null;
  onclose: ((ev: unknown) => void) | null;
  onerror: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
}

type WSCtor = new (url: string) => WSLike;

function resolveWSImpl(): WSCtor {
  // Browser / edge runtimes expose a global WebSocket. Prefer it when present
  // so bundlers don't drag `ws` into the browser build.
  const g = globalThis as unknown as { WebSocket?: WSCtor };
  if (typeof g.WebSocket !== "undefined") {
    return g.WebSocket;
  }
  // Node.js fallback. `ws` is only required in Node contexts.
  // eslint-disable-next-line @typescript-eslint/no-var-requires
  const mod = require("ws");
  return (mod.WebSocket ?? mod.default ?? mod) as WSCtor;
}

function parsePortFromUrl(url: string): number {
  // Extract the port out of a ws:// or wss:// URL. WHATWG URL normalises
  // default-port removal (80 for ws, 443 for wss), so fall back to those
  // when `port` comes back empty.
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    throw new Error(`MlinkClient: invalid url '${url}'`);
  }
  if (parsed.port) {
    const n = Number(parsed.port);
    if (Number.isFinite(n) && n > 0) {
      return n;
    }
  }
  if (parsed.protocol === "wss:" || parsed.protocol === "https:") {
    return 443;
  }
  if (parsed.protocol === "ws:" || parsed.protocol === "http:") {
    return 80;
  }
  throw new Error(`MlinkClient: cannot derive port from url '${url}'`);
}

function readDaemonInfo(overridePath?: string): { port: number } {
  // Browser guard — the caller must provide `url` there.
  const isNode =
    typeof process !== "undefined" &&
    !!(process as unknown as { versions?: { node?: string } }).versions?.node;
  if (!isNode) {
    throw new Error(
      "MlinkClient: no url provided and not running under Node — pass options.url in browser environments",
    );
  }
  // eslint-disable-next-line @typescript-eslint/no-var-requires
  const { readFileSync } = require("fs") as typeof import("fs");
  // eslint-disable-next-line @typescript-eslint/no-var-requires
  const { join } = require("path") as typeof import("path");
  // eslint-disable-next-line @typescript-eslint/no-var-requires
  const { homedir } = require("os") as typeof import("os");
  const envOverride = process.env.MLINK_DAEMON_FILE;
  const path = overridePath ?? envOverride ?? join(homedir(), ".mlink", "daemon.json");
  const raw = readFileSync(path, "utf8");
  const parsed = JSON.parse(raw) as { port?: unknown };
  if (typeof parsed.port !== "number" || !Number.isFinite(parsed.port)) {
    throw new Error(`MlinkClient: daemon.json at ${path} missing numeric 'port'`);
  }
  return { port: parsed.port };
}

interface PendingRequest {
  resolve: () => void;
  reject: (err: Error) => void;
  timer: ReturnType<typeof setTimeout>;
  type: "hello" | "join" | "leave" | "send";
}

// Well-typed EventEmitter surface for TypeScript consumers.
export interface MlinkClient {
  on<E extends keyof ClientEvents>(event: E, listener: ClientEvents[E]): this;
  once<E extends keyof ClientEvents>(event: E, listener: ClientEvents[E]): this;
  off<E extends keyof ClientEvents>(event: E, listener: ClientEvents[E]): this;
  emit<E extends keyof ClientEvents>(event: E, ...args: Parameters<ClientEvents[E]>): boolean;
}

/**
 * WebSocket client for mlink daemon.
 *
 * Lifecycle: `connect()` opens the socket, waits for `ready`, and resolves.
 * `join` / `leave` / `send` issue request frames with an auto-incremented id
 * and resolve when the daemon acks (or reject on `error`). Incoming `message`
 * and `room_state` frames emit events.
 *
 * On an unexpected socket close, if `autoReconnect` is enabled the client
 * reconnects with 1/2/4/8…s exponential backoff (capped at `maxReconnectDelayMs`)
 * and re-joins any rooms that were active before the drop.
 */
export class MlinkClient extends EventEmitter {
  private readonly url: string;
  private readonly _port: number;
  private readonly clientName?: string;
  private readonly pingIntervalMs: number;
  private readonly requestTimeoutMs: number;
  private readonly autoReconnect: boolean;
  private readonly maxReconnectDelayMs: number;

  private ws: WSLike | null = null;
  private nextId = 1;
  private readonly pending = new Map<string, PendingRequest>();
  private readonly joinedRooms = new Set<string>();
  private readonly roomPeers = new Map<string, Peer[]>();
  private _appUuid = "";
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private reconnectAttempt = 0;
  private closedByUser = false;
  private connectPromise: Promise<void> | null = null;

  constructor(options: ClientOptions = {}) {
    super();
    // EventEmitter throws synchronously on `emit('error', ...)` when no
    // listener is attached. A connect failure, socket error, or daemon-sent
    // error frame arriving before the caller has wired up an 'error' handler
    // would crash the process — swallow the default behaviour. Callers who
    // care attach their own listener.
    this.on("error", () => {});
    if (options.url) {
      this.url = options.url;
      this._port = parsePortFromUrl(options.url);
    } else {
      const { port } = readDaemonInfo(options.daemonInfoPath);
      this.url = `ws://127.0.0.1:${port}/ws`;
      this._port = port;
    }
    this.clientName = options.clientName;
    this.pingIntervalMs = options.pingIntervalMs ?? 30_000;
    this.requestTimeoutMs = options.requestTimeoutMs ?? 10_000;
    this.autoReconnect = options.autoReconnect ?? true;
    this.maxReconnectDelayMs = options.maxReconnectDelayMs ?? 30_000;
  }

  get appUuid(): string {
    return this._appUuid;
  }

  /**
   * Daemon port this client is pointed at. Resolved at construction time
   * from either the explicit `url` option or `~/.mlink/daemon.json`. Stable
   * for the lifetime of the instance.
   */
  get port(): number {
    return this._port;
  }

  get rooms(): string[] {
    return Array.from(this.joinedRooms);
  }

  peers(room: string): Peer[] {
    return this.roomPeers.get(room) ?? [];
  }

  isConnected(): boolean {
    // 1 === OPEN (both Node ws and browser WebSocket).
    return this.ws !== null && this.ws.readyState === 1;
  }

  /** Open the WS connection and resolve once the daemon sends `ready`. */
  connect(): Promise<void> {
    if (this.connectPromise) {
      return this.connectPromise;
    }
    this.closedByUser = false;
    this.connectPromise = this.openSocket();
    // Clear the cache once the promise settles so a later reconnect can
    // produce a fresh one.
    this.connectPromise.finally(() => {
      this.connectPromise = null;
    });
    return this.connectPromise;
  }

  private openSocket(): Promise<void> {
    return new Promise((resolve, reject) => {
      let settled = false;
      const Ctor = resolveWSImpl();
      let ws: WSLike;
      try {
        ws = new Ctor(this.url);
      } catch (e) {
        reject(e instanceof Error ? e : new Error(String(e)));
        return;
      }
      this.ws = ws;

      ws.onopen = () => {
        // Daemon sends the initial `ready` without prompting. We still send a
        // `hello` right away to register client_name and to get a second
        // id-correlated ready/ack when the caller wants one. The first
        // unsolicited ready resolves connect().
        const payload: Record<string, string> = {};
        if (this.clientName) {
          payload.client_name = this.clientName;
        }
        this.sendRaw({ v: WS_PROTOCOL_VERSION, type: "hello", payload });
        this.emit("connected");
      };

      ws.onerror = (ev: unknown) => {
        // In browsers the error event is opaque. Surface something useful.
        const msg =
          (ev as { message?: string } | null)?.message ?? "websocket error";
        if (!settled) {
          settled = true;
          reject(new Error(msg));
        }
        this.emit("error", { code: "socket_error", message: msg });
      };

      ws.onclose = () => {
        this.cleanupSocket();
        this.emit("disconnected");
        if (!settled) {
          settled = true;
          reject(new Error("websocket closed before ready"));
        }
        if (this.autoReconnect && !this.closedByUser) {
          this.scheduleReconnect();
        }
      };

      ws.onmessage = (ev: { data: unknown }) => {
        const text = typeof ev.data === "string" ? ev.data : String(ev.data);
        this.handleFrame(text, (info) => {
          if (!settled) {
            settled = true;
            resolve();
            this.emit("ready", info);
            this.startHeartbeat();
            // Re-join any rooms from a previous session (reconnect path).
            for (const room of this.joinedRooms) {
              this.sendJoinFrame(room);
            }
          } else {
            this.emit("ready", info);
          }
        });
      };
    });
  }

  private scheduleReconnect(): void {
    if (this.reconnectTimer) {
      return;
    }
    const base = Math.min(
      this.maxReconnectDelayMs,
      1000 * 2 ** Math.min(this.reconnectAttempt, 5),
    );
    this.reconnectAttempt += 1;
    this.emit("reconnecting", this.reconnectAttempt, base);
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.openSocket().then(
        () => {
          this.reconnectAttempt = 0;
        },
        () => {
          // openSocket already emitted error/disconnected; another reconnect
          // will be scheduled from the onclose handler.
        },
      );
    }, base);
  }

  /** Close the socket and stop reconnecting. Safe to call when not connected. */
  disconnect(): void {
    this.closedByUser = true;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      try {
        this.ws.close();
      } catch {
        // ignore — we're tearing down.
      }
    }
    this.cleanupSocket();
  }

  private cleanupSocket(): void {
    if (this.pingTimer) {
      clearInterval(this.pingTimer);
      this.pingTimer = null;
    }
    if (this.ws) {
      this.ws.onopen = null;
      this.ws.onclose = null;
      this.ws.onerror = null;
      this.ws.onmessage = null;
      this.ws = null;
    }
    // Any in-flight request can never be acked now.
    for (const [id, req] of this.pending) {
      clearTimeout(req.timer);
      req.reject(new Error("connection closed"));
      this.pending.delete(id);
    }
  }

  private startHeartbeat(): void {
    if (this.pingIntervalMs <= 0) {
      return;
    }
    if (this.pingTimer) {
      clearInterval(this.pingTimer);
    }
    this.pingTimer = setInterval(() => {
      if (this.isConnected()) {
        this.sendRaw({ v: WS_PROTOCOL_VERSION, type: "ping", payload: {} });
      }
    }, this.pingIntervalMs);
    // Let Node exit even if the timer is still armed.
    const t = this.pingTimer as unknown as { unref?: () => void };
    t.unref?.();
  }

  /** Join a room. Resolves on ack, rejects on error or timeout. */
  join(room: string): Promise<void> {
    return this.request("join", { room }).then(() => {
      this.joinedRooms.add(room);
    });
  }

  /** Leave a room. Resolves on ack. */
  leave(room: string): Promise<void> {
    return this.request("leave", { room }).then(() => {
      this.joinedRooms.delete(room);
      this.roomPeers.delete(room);
    });
  }

  /**
   * Send a JSON payload to a room. When `to` is omitted the daemon fans the
   * message to every currently-connected peer; when supplied the daemon
   * addresses a single peer by app_uuid.
   */
  send(room: string, payload: unknown, to?: string): Promise<void> {
    const body: { room: string; payload: unknown; to?: string } = { room, payload };
    if (to) {
      body.to = to;
    }
    return this.request("send", body);
  }

  private request(
    type: "join" | "leave" | "send",
    payload: unknown,
  ): Promise<void> {
    if (!this.isConnected()) {
      return Promise.reject(new Error("not connected"));
    }
    const id = `req-${this.nextId++}`;
    return new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`request '${type}' timed out after ${this.requestTimeoutMs}ms`));
      }, this.requestTimeoutMs);
      const t = timer as unknown as { unref?: () => void };
      t.unref?.();
      this.pending.set(id, { resolve, reject, timer, type });
      this.sendRaw({ v: WS_PROTOCOL_VERSION, id, type, payload });
    });
  }

  // Fire-and-forget join used during reconnect. We don't want to surface
  // unhandled-rejection noise if the socket drops again before ack.
  private sendJoinFrame(room: string): void {
    this.request("join", { room }).catch(() => {
      // swallow — reconnect logic will retry on the next cycle.
    });
  }

  private sendRaw(obj: unknown): void {
    if (!this.ws || this.ws.readyState !== 1) {
      return;
    }
    this.ws.send(JSON.stringify(obj));
  }

  private handleFrame(text: string, onReady: (info: ReadyInfo) => void): void {
    let frame: RawInFrame;
    try {
      frame = JSON.parse(text) as RawInFrame;
    } catch (e) {
      this.emit("error", {
        code: "bad_json",
        message: `failed to parse frame: ${(e as Error).message}`,
      });
      return;
    }
    if (frame.v !== WS_PROTOCOL_VERSION) {
      this.emit("error", {
        code: "bad_version",
        message: `daemon sent version ${frame.v}, expected ${WS_PROTOCOL_VERSION}`,
      });
      return;
    }
    switch (frame.type) {
      case "ready": {
        this._appUuid = frame.payload.app_uuid;
        // If `hello` was sent with an id, daemon also acks it; we don't wait.
        onReady({ appUuid: frame.payload.app_uuid, version: frame.payload.version });
        return;
      }
      case "ack": {
        // Correlate by either top-level id or payload.id — daemon sets both.
        const id = frame.id ?? frame.payload.id;
        if (!id) return;
        const req = this.pending.get(id);
        if (!req) return;
        this.pending.delete(id);
        clearTimeout(req.timer);
        req.resolve();
        return;
      }
      case "error": {
        const err: MlinkError = {
          code: frame.payload.code,
          message: frame.payload.message,
        };
        const id = frame.id ?? frame.payload.id;
        if (id) {
          const req = this.pending.get(id);
          if (req) {
            this.pending.delete(id);
            clearTimeout(req.timer);
            req.reject(
              Object.assign(new Error(`${err.code}: ${err.message}`), err),
            );
            return;
          }
        }
        this.emit("error", err);
        return;
      }
      case "room_state": {
        const state: RoomState = {
          room: frame.payload.room,
          peers: frame.payload.peers,
          joined: frame.payload.joined,
        };
        if (state.joined) {
          this.roomPeers.set(state.room, state.peers);
          this.joinedRooms.add(state.room);
        } else {
          this.roomPeers.delete(state.room);
          this.joinedRooms.delete(state.room);
        }
        this.emit("room_state", state);
        return;
      }
      case "message": {
        const msg: Message = {
          room: frame.payload.room,
          from: frame.payload.from,
          payload: frame.payload.payload,
          ts: frame.payload.ts,
        };
        this.emit("message", msg);
        return;
      }
      case "pong": {
        // Heartbeat — nothing to do. An id-correlated pong is allowed to
        // resolve a pending ping request, but we don't expose ping() so no
        // pending entry exists.
        return;
      }
      default: {
        // Unknown frame type — ignore. A forward-compatible daemon might send
        // new types; erroring here would break old clients talking to new
        // daemons.
        return;
      }
    }
  }
}
