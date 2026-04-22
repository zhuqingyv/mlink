// mlink WS protocol v1 types.
// Wire envelope: {"v": 1, "id"?, "type": ..., "payload": {...}}.

export const WS_PROTOCOL_VERSION = 1;

export interface Peer {
  app_uuid: string;
  name: string;
}

export interface ReadyInfo {
  appUuid: string;
  version: string;
}

export interface Message {
  room: string;
  from: string;
  payload: unknown;
  ts: number;
}

export interface RoomState {
  room: string;
  peers: Peer[];
  joined: boolean;
}

export interface MlinkError {
  code: string;
  message: string;
}

export interface ClientOptions {
  /** Full ws URL. When omitted, Node reads `~/.mlink/daemon.json` to find the port. */
  url?: string;
  /** Override the path to daemon.json. Node only. */
  daemonInfoPath?: string;
  /** Client name sent in the `hello` frame (informational, logged by daemon). */
  clientName?: string;
  /** Milliseconds between ping frames. 0 disables heartbeat. Default 30_000. */
  pingIntervalMs?: number;
  /** Request timeout for join/leave/send (awaiting ack). Default 10_000. */
  requestTimeoutMs?: number;
  /** Enable auto-reconnect with exponential backoff. Default true. */
  autoReconnect?: boolean;
  /** Max backoff delay in ms. Default 30_000. */
  maxReconnectDelayMs?: number;
}

export type RawInFrame =
  | { v: number; type: "ready"; id?: string; payload: { app_uuid: string; version: string } }
  | { v: number; type: "ack"; id?: string; payload: { id: string; type: string } }
  | { v: number; type: "error"; id?: string; payload: { id?: string; code: string; message: string } }
  | { v: number; type: "room_state"; id?: string; payload: { room: string; peers: Peer[]; joined: boolean } }
  | { v: number; type: "message"; id?: string; payload: { room: string; from: string; payload: unknown; ts: number } }
  | { v: number; type: "pong"; id?: string; payload: Record<string, unknown> };

export interface ClientEvents {
  ready: (info: ReadyInfo) => void;
  message: (msg: Message) => void;
  room_state: (state: RoomState) => void;
  error: (err: MlinkError) => void;
  disconnected: () => void;
  reconnecting: (attempt: number, delayMs: number) => void;
  connected: () => void;
}
