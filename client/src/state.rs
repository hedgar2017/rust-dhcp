//! The DHCP client state module.

use std::{
    fmt,
    net::{
        SocketAddr,
        IpAddr,
        Ipv4Addr,
    },
    time::{
        Instant,
        Duration,
    },
};

use tokio::timer::Delay;
use chrono::prelude::*;
use rand;

use protocol::DHCP_PORT_SERVER;

use backoff::Backoff;
use forthon::Forthon;

/// Initial timeout in seconds for the BEB timers.
const BACKOFF_TIMEOUT_INITIAL: u64 = 4;
/// Maximum timeout in seconds for the BEB timers.
const BACKOFF_TIMEOUT_MAXIMUM: u64 = 64;
/// Minimal stimeout in seconds for the BEF™ timers.
const FORTHON_TIMEOUT_MINIMAL: u64 = 60;
/// Is used if a server does not provide the `renewal_time` option.
const RENEWAL_TIME_FACTOR: f64 = 0.5;
/// Is used if a server does not provide the `rebinding_time` option.
const REBINDING_TIME_FACTOR: f64 = 0.875;

/// RFC 2131 DHCP states.
#[derive(Clone, Copy)]
pub enum DhcpState {
    Init,
    Selecting,
    Requesting,
    InitReboot,
    Rebooting,
    Bound,
    Renewing,
    Rebinding,
}

impl fmt::Display for DhcpState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::DhcpState::*;
        match self {
            Init => write!(f, "INIT"),
            Selecting => write!(f, "SELECTING"),
            Requesting => write!(f, "REQUESTING"),
            InitReboot => write!(f, "INITREBOOT"),
            Rebooting => write!(f, "REBOOTING"),
            Bound => write!(f, "BOUND"),
            Renewing => write!(f, "RENEWING"),
            Rebinding => write!(f, "REBINDING"),
        }
    }
}

/// Mutable `Client` data.
pub struct State {
    /// The destination address, usually `255.255.255.255` or a known server address.
    destination     : SocketAddr,
    /// Current DHCP client state (RFC 2131).
    dhcp_state      : DhcpState,
    /// If the client requires broadcast response (e.g. if it is not configured yet).
    is_broadcast    : bool,
    /// Generated by the client for each session.
    transaction_id  : u32,
    /// `SELECTING` state controller.
    discover_sent   : bool,
    /// Recorded by the client from the selected `DHCPOFFER`.
    offered_address : Ipv4Addr,
    /// Recorded by the client from the selected `DHCPOFFER`.
    offered_time    : u32,
    /// The address of the server selected from a `DHCPOFFER`.
    dhcp_server_id  : Option<Ipv4Addr>,
    /// `REQUESTING`, `REBOOTING`, `RENEWING` and `REBINDING` states controller.
    request_sent    : bool,
    /// Recorded by the client right before sending the `DhcpRequest`.
    requested_at    : i64,
    /// Recorded by the client from the `DhcpAck`.
    assigned_address: Ipv4Addr,

    /// Seconds from `BOUND` till `RENEWING` state.
    renewal_after   : u64,
    /// Seconds from `RENEWING` till `REBINDING` state.
    rebinding_after : u64,
    /// Seconds from `REBINDING` till lease expiration.
    expiration_after: u64,

    /// DHCPOFFER receive deadline.
    pub timer_offer     : Option<Backoff>,
    /// DHCPACK or DHCPNAK receive deadline.
    pub timer_ack       : Option<Backoff>,
    /// Renewal timer (so called T1 in RFC 2131).
    pub timer_renewal   : Option<Delay>,
    /// Rebinding timer (so called T2 in RFC 2131).
    pub timer_rebinding : Option<Forthon>,
    /// Lease expiration timer.
    pub timer_expiration: Option<Forthon>,
}

impl State {
    /// Constructs a default state.
    pub fn new(
        destination     : SocketAddr,
        dhcp_state      : DhcpState,
        is_broadcast    : bool,
    ) -> Self {
        State {
            destination,
            dhcp_state,
            is_broadcast,
            transaction_id      : rand::random::<u32>(),
            discover_sent       : false,
            offered_address     : Ipv4Addr::new(0,0,0,0),
            offered_time        : 0u32,
            dhcp_server_id      : None,
            request_sent        : false,
            requested_at        : 0i64,
            assigned_address    : Ipv4Addr::new(0,0,0,0),

            renewal_after       : 0u64,
            rebinding_after     : 0u64,
            expiration_after    : 0u64,

            timer_offer         : None,
            timer_ack           : None,
            timer_renewal       : None,
            timer_rebinding     : None,
            timer_expiration    : None,
        }
    }

    pub fn destination(&self) -> SocketAddr {
        self.destination.to_owned()
    }

    pub fn dhcp_state(&self) -> DhcpState {
        self.dhcp_state
    }

    pub fn is_broadcast(&self) -> bool {
        self.is_broadcast
    }

    pub fn xid(&self) -> u32 {
        self.transaction_id
    }

    pub fn offered_address(&self) -> Ipv4Addr {
        self.offered_address.to_owned()
    }

    pub fn offered_time(&self) -> u32 {
        self.offered_time
    }

    pub fn dhcp_server_id(&self) -> Option<Ipv4Addr> {
        self.dhcp_server_id
    }

    pub fn assigned_address(&self) -> Ipv4Addr {
        self.assigned_address.to_owned()
    }

    pub fn is_discover_sent(&self) -> bool {
        self.discover_sent
    }

    pub fn set_discover_sent(&mut self, value: bool) {
        self.discover_sent = value;
    }

    pub fn is_request_sent(&self) -> bool {
        self.request_sent
    }

    pub fn set_request_sent(&mut self, value: bool) {
        self.record_request_time();
        self.request_sent = value;
    }

    pub fn init_to_selecting(&mut self) {
        info!("Changing state from {} to {}", DhcpState::Init, DhcpState::Selecting);
        self.set_discover_sent(false);
        self.run_timer_offer();
        self.dhcp_server_id = None;
        self.dhcp_state = DhcpState::Selecting;
    }

    pub fn selecting_to_requesting(
        &mut self,
        offered_address: Ipv4Addr,
        offered_time: u32,
        dhcp_server_id: Option<Ipv4Addr>,
    ) {
        info!("Changing state from {} to {}", DhcpState::Selecting, DhcpState::Requesting);
        self.set_destination(dhcp_server_id);
        self.set_request_sent(false);
        self.set_offered_address(offered_address);
        self.set_offered_time(offered_time);
        self.set_dhcp_server_id(dhcp_server_id);
        self.record_request_time();
        self.run_timer_ack();
        self.dhcp_state = DhcpState::Requesting;
    }

    pub fn requesting_to_init(&mut self) {
        info!("Changing state from {} to {}", DhcpState::Requesting, DhcpState::Init);
        self.set_destination(None);
        self.dhcp_server_id = None;
        self.dhcp_state = DhcpState::Init;
    }

    pub fn requesting_to_bound(
        &mut self,
        assigned_address: Ipv4Addr,
        renewal_time: Option<u32>,
        rebinding_time: Option<u32>,
        expiration_time: u32,
    ) {
        info!("Changing state from {} to {}", DhcpState::Requesting, DhcpState::Bound);
        self.set_assigned_address(assigned_address);
        self.set_times(renewal_time, rebinding_time, expiration_time);
        self.set_broadcast(false);
        self.run_timer_renewal();
        self.dhcp_state = DhcpState::Bound;
    }

    pub fn initreboot_to_rebooting(&mut self) {
        info!("Changing state from {} to {}", DhcpState::InitReboot, DhcpState::Rebooting);
        self.set_request_sent(false);
        self.record_request_time();
        self.run_timer_ack();
        self.dhcp_state = DhcpState::Rebooting;
    }

    pub fn rebooting_to_init(&mut self) {
        info!("Changing state from {} to {}", DhcpState::Rebooting, DhcpState::Init);
        self.set_destination(None);
        self.timer_offer = None;
        self.dhcp_state = DhcpState::Init;
    }

    pub fn rebooting_to_bound(
        &mut self,
        assigned_address: Ipv4Addr,
        renewal_time: Option<u32>,
        rebinding_time: Option<u32>,
        expiration_time: u32,
        dhcp_server_id: Option<Ipv4Addr>,
    ) {
        info!("Changing state from {} to {}", DhcpState::Rebooting, DhcpState::Bound);
        self.set_assigned_address(assigned_address);
        self.set_dhcp_server_id(dhcp_server_id);
        self.set_times(renewal_time, rebinding_time, expiration_time);
        self.run_timer_renewal();
        self.dhcp_state = DhcpState::Bound;
    }

    pub fn bound_to_renewing(&mut self) {
        info!("Changing state from {} to {}", DhcpState::Bound, DhcpState::Renewing);
        self.run_timer_rebinding();
        self.set_request_sent(false);
        self.dhcp_state = DhcpState::Renewing;
    }

    pub fn renewing_to_bound(
        &mut self,
        assigned_address: Ipv4Addr,
        renewal_time: Option<u32>,
        rebinding_time: Option<u32>,
        expiration_time: u32,
    ) {
        info!("Changing state from {} to {}", DhcpState::Renewing, DhcpState::Bound);
        self.set_assigned_address(assigned_address);
        self.set_times(renewal_time, rebinding_time, expiration_time);
        self.run_timer_renewal();
        self.dhcp_state = DhcpState::Bound;
    }

    pub fn renewing_to_rebinding(&mut self) {
        info!("Changing state from {} to {}", DhcpState::Renewing, DhcpState::Rebinding);
        self.run_timer_expiration();
        self.set_request_sent(false);
        self.set_destination(None);
        self.dhcp_state = DhcpState::Rebinding;
    }

    pub fn rebinding_to_init(&mut self) {
        info!("Changing state from {} to {}", DhcpState::Rebinding, DhcpState::Init);
        self.set_destination(None);
        self.set_broadcast(true);
        self.dhcp_state = DhcpState::Init;
    }

    pub fn rebinding_to_bound(
        &mut self,
        assigned_address: Ipv4Addr,
        renewal_time: Option<u32>,
        rebinding_time: Option<u32>,
        expiration_time: u32,
    ) {
        info!("Changing state from {} to {}", DhcpState::Rebinding, DhcpState::Bound);
        self.set_assigned_address(assigned_address);
        self.set_request_sent(false);
        self.set_times(renewal_time, rebinding_time, expiration_time);
        self.set_broadcast(false);
        self.run_timer_renewal();
        self.dhcp_state = DhcpState::Bound;
    }

    fn set_destination(&mut self, ip: Option<Ipv4Addr>) {
        self.destination = SocketAddr::new(
            IpAddr::V4(ip.unwrap_or(Ipv4Addr::new(255,255,255,255))),
            DHCP_PORT_SERVER,
        );
    }

    fn set_broadcast(&mut self, value: bool) {
        self.is_broadcast = value;
    }

    fn set_offered_address(&mut self, value: Ipv4Addr) {
        self.offered_address = value;
    }

    fn set_offered_time(&mut self, value: u32) {
        self.offered_time = value;
    }

    fn record_request_time(&mut self) {
        self.requested_at = Utc::now().timestamp();
    }

    fn set_dhcp_server_id(&mut self, value: Option<Ipv4Addr>) {
        self.dhcp_server_id = value;
    }

    fn set_assigned_address(&mut self, value: Ipv4Addr) {
        self.assigned_address = value;
    }

    fn set_times(
        &mut self,
        renewal_time: Option<u32>,
        rebinding_time: Option<u32>,
        expiration_time: u32,
    ) {
        let renewal_time = renewal_time.unwrap_or(((expiration_time as f64) * RENEWAL_TIME_FACTOR) as u32);
        let rebinding_time = rebinding_time.unwrap_or(((expiration_time as f64) * REBINDING_TIME_FACTOR) as u32);

        self.renewal_after = ((renewal_time as i64) - (Utc::now().timestamp() - self.requested_at)) as u64;
        self.rebinding_after = (rebinding_time as u64) - self.renewal_after;
        self.expiration_after = (expiration_time as u64) - self.renewal_after - self.rebinding_after;
    }

    fn run_timer_offer(&mut self) {
        self.timer_offer = Some(Backoff::new(
            Duration::from_secs(BACKOFF_TIMEOUT_INITIAL),
            Duration::from_secs(BACKOFF_TIMEOUT_MAXIMUM),
        ));
    }

    fn run_timer_ack(&mut self) {
        self.timer_ack = Some(Backoff::new(
            Duration::from_secs(BACKOFF_TIMEOUT_INITIAL),
            Duration::from_secs(BACKOFF_TIMEOUT_MAXIMUM),
        ));
    }

    fn run_timer_renewal(&mut self) {
        self.timer_renewal = Some(Delay::new(
            Instant::now() + Duration::from_secs(self.renewal_after)
        ));
    }

    fn run_timer_rebinding(&mut self) {
        self.timer_rebinding = Some(Forthon::new(
            Duration::from_secs(self.rebinding_after),
            Duration::from_secs(FORTHON_TIMEOUT_MINIMAL),
        ));
    }

    fn run_timer_expiration(&mut self) {
        self.timer_expiration = Some(Forthon::new(
            Duration::from_secs(self.expiration_after),
            Duration::from_secs(FORTHON_TIMEOUT_MINIMAL),
        ));
    }
}