//! Local-network discovery (mDNS, service `_clipet._udp`).
//!
//! Always on while Clipet Connect runs, so paired devices find each other's
//! current LAN address without the internet. While Add Device is open, the
//! announcement also carries "available to pair" and the device name — on
//! the local network only: the wrapper adds the label to what mDNS
//! publishes, never to the endpoint's own data, which also goes to the
//! public address-lookup service.

use iroh::EndpointId;
use iroh::address_lookup::{AddressLookup, EndpointData, Error, Item};
use iroh::endpoint_info::UserData;
use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use n0_future::boxed::BoxStream;
use n0_future::{Stream, StreamExt};
use std::sync::{Arc, Mutex, RwLock};

const SERVICE: &str = "clipet";
pub const PAIR_LABEL: &str = "clipet-pair:";

#[derive(Debug, Clone)]
pub struct Discovery {
    mdns: MdnsAddressLookup,
    label: Arc<RwLock<Option<UserData>>>,
    last: Arc<Mutex<Option<EndpointData>>>,
}

impl Discovery {
    /// `None` when the network has no usable interface for mDNS.
    pub fn new(id: EndpointId) -> Option<Self> {
        let mdns = MdnsAddressLookup::builder().service_name(SERVICE).build(id).ok()?;
        Some(Self { mdns, label: Arc::new(RwLock::new(None)), last: Arc::new(Mutex::new(None)) })
    }

    /// Announces this device as available to pair (with its name), or stops.
    pub fn set_pairing_name(&self, name: Option<&str>) {
        let label = name.and_then(|n| {
            let mut text = format!("{PAIR_LABEL}{n}");
            while text.len() > UserData::MAX_LENGTH {
                text.pop();
            }
            text.parse::<UserData>().ok()
        });
        *self.label.write().unwrap() = label;
        if let Some(data) = self.last.lock().unwrap().clone() {
            self.publish_labelled(&data);
        }
    }

    fn publish_labelled(&self, data: &EndpointData) {
        let mut data = data.clone();
        data.set_user_data(self.label.read().unwrap().clone());
        self.mdns.publish(&data);
    }

    pub async fn events(&self) -> impl Stream<Item = DiscoveryEvent> + Unpin + use<> {
        self.mdns.subscribe().await
    }
}

impl AddressLookup for Discovery {
    fn publish(&self, data: &EndpointData) {
        *self.last.lock().unwrap() = Some(data.clone());
        self.publish_labelled(data);
    }

    fn resolve(&self, endpoint_id: EndpointId) -> Option<BoxStream<Result<Item, Error>>> {
        self.mdns.resolve(endpoint_id)
    }
}

/// The name a device announces while it is available to pair.
pub fn pairing_name(event: &DiscoveryEvent) -> Option<(EndpointId, Option<String>)> {
    match event {
        DiscoveryEvent::Discovered { endpoint_info, .. } => {
            let name = endpoint_info
                .data
                .user_data()
                .and_then(|u| u.as_ref().strip_prefix(PAIR_LABEL).map(super::identity::clean_name));
            Some((endpoint_info.endpoint_id, name))
        }
        DiscoveryEvent::Expired { endpoint_id } => Some((*endpoint_id, None)),
        _ => None,
    }
}

/// Keeps `on_change` informed of nearby devices that are available to pair.
pub async fn watch(discovery: Discovery, on_change: impl Fn(EndpointId, Option<String>) + Send + 'static) {
    let mut events = discovery.events().await;
    while let Some(event) = events.next().await {
        if let Some((id, name)) = pairing_name(&event) {
            on_change(id, name);
        }
    }
}
