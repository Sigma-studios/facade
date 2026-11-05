//! Ubuntu: apt for updates, `/run/reboot-required` for reboots.

use facade_core::events::{Reboot, Updates};
use facade_core::platform::{Checked, OsId, Support};

use super::{Platform, pkg, reboot};

pub struct Ubuntu {
    os: OsId,
}

impl Ubuntu {
    pub fn new(os: OsId) -> Self {
        Self { os }
    }
}

#[async_trait::async_trait]
impl Platform for Ubuntu {
    fn os(&self) -> &OsId {
        &self.os
    }

    fn support(&self) -> Support {
        Support::Supported
    }

    fn tool_hint(&self) -> &str {
        "apt install e2fsprogs mount"
    }

    async fn updates(&self) -> Checked<Updates> {
        pkg::apt::updates().await
    }

    async fn reboot(&self) -> Checked<Reboot> {
        Checked::Known(reboot::required_file())
    }
}
