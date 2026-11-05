//! Any other Linux: apps run, and the host checks facade cannot do here say so
//! rather than reading as "nothing to do".

use facade_core::events::{Reboot, Updates};
use facade_core::platform::{Checked, OsId, Support};

use super::Platform;

pub struct Generic {
    os: OsId,
}

impl Generic {
    pub fn new(os: OsId) -> Self {
        Self { os }
    }

    fn unsupported<T>(&self) -> Checked<T> {
        Checked::Unsupported(format!(
            "facade supports Ubuntu and NixOS; this node runs {}",
            self.os.name()
        ))
    }
}

#[async_trait::async_trait]
impl Platform for Generic {
    fn os(&self) -> &OsId {
        &self.os
    }

    fn support(&self) -> Support {
        Support::BestEffort
    }

    async fn updates(&self) -> Checked<Updates> {
        self.unsupported()
    }

    async fn reboot(&self) -> Checked<Reboot> {
        self.unsupported()
    }
}
