pub(crate) use dwo_protocol::is_side_effect_method;

use super::Host;

impl Host {
    pub(crate) fn management_capabilities(&self) -> dwo_protocol::ManagementCapabilities {
        dwo_protocol::capabilities()
    }
}
