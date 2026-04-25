//! `mlink trust list/remove` — thin operator on the on-disk `TrustStore`.
//! The store lives at `TrustStore::default_path()` (typically under
//! `~/.mlink/`); we open it fresh every invocation because the CLI is
//! stateless.

use mlink_core::core::security::TrustStore;
use mlink_core::protocol::errors::MlinkError;

use mlink_cli::TrustAction;

pub(crate) async fn cmd_trust(action: TrustAction) -> Result<(), MlinkError> {
    let path = TrustStore::default_path()?;
    let mut store = TrustStore::new(path.clone())?;
    match action {
        TrustAction::List => {
            let peers = store.list();
            if peers.is_empty() {
                println!("no trusted peers (store: {})", path.display());
                return Ok(());
            }
            println!("trusted peers ({} total, store: {}):", peers.len(), path.display());
            println!("{:<40}  {:<24}  {}", "APP_UUID", "NAME", "TRUSTED_AT");
            for p in peers {
                println!("{:<40}  {:<24}  {}", p.app_uuid, p.name, p.trusted_at);
            }
        }
        TrustAction::Remove { peer_id } => {
            if !store.is_trusted(&peer_id) {
                println!("peer {peer_id} was not trusted");
                return Ok(());
            }
            store.remove(&peer_id)?;
            println!("removed {peer_id} from trust store");
        }
    }
    Ok(())
}
