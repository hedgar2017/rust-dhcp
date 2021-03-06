//! The DHCP client state module.

use std::{
    fmt,
    net::Ipv4Addr,
    time::{Duration, Instant},
};

use chrono::prelude::*;
use rand;
use tokio::timer::Delay;

use dhcp_protocol::Message;

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
///
/// The ones end with `Sent` are not described in RFC 2131 and
/// are just substates to tell if the request has been sent or not.
#[derive(Clone, Copy)]
pub enum DhcpState {
    Init,
    Selecting,
    SelectingSent,
    Requesting,
    RequestingSent,
    InitReboot,
    Rebooting,
    RebootingSent,
    Bound,
    Renewing,
    RenewingSent,
    Rebinding,
    RebindingSent,
}

impl fmt::Display for DhcpState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::DhcpState::*;
        match self {
            Init => write!(f, "INIT"),
            Selecting => write!(f, "SELECTING"),
            SelectingSent => write!(f, "SELECTING_SENT"),
            Requesting => write!(f, "REQUESTING"),
            RequestingSent => write!(f, "REQUESTING_SENT"),
            InitReboot => write!(f, "INITREBOOT"),
            Rebooting => write!(f, "REBOOTING"),
            RebootingSent => write!(f, "REBOOTING_SENT"),
            Bound => write!(f, "BOUND"),
            Renewing => write!(f, "RENEWING"),
            RenewingSent => write!(f, "RENEWING_SENT"),
            Rebinding => write!(f, "REBINDING"),
            RebindingSent => write!(f, "REBINDING_SENT"),
        }
    }
}

/// Mutable `Client` data.
pub struct State {
    /// Current DHCP client state (RFC 2131).
    dhcp_state: DhcpState,
    /// If the client requires broadcast response (e.g. if it is not configured yet).
    is_broadcast: bool,
    /// Generated by the client for each session.
    transaction_id: u32,
    /// Recorded by the client from the selected `DHCPOFFER`.
    offered_address: Ipv4Addr,
    /// Recorded by the client from the selected `DHCPOFFER`.
    offered_time: u32,
    /// The address of the server selected from a `DHCPOFFER`.
    dhcp_server_id: Option<Ipv4Addr>,
    /// Recorded by the client from the `DhcpAck`.
    assigned_address: Ipv4Addr,

    /// Recorded by the client right before sending the `DhcpRequest`.
    requested_at: i64,
    /// Seconds from `BOUND` till `RENEWING` state.
    renewal_after: u64,
    /// Seconds from `RENEWING` till `REBINDING` state.
    rebinding_after: u64,
    /// Seconds from `REBINDING` till lease expiration.
    expiration_after: u64,

    /// DHCPOFFER receive deadline.
    pub timer_offer: Option<Backoff>,
    /// DHCPACK or DHCPNAK receive deadline.
    pub timer_ack: Option<Backoff>,
    /// Renewal timer (so called T1 in RFC 2131).
    pub timer_renewal: Option<Delay>,
    /// Rebinding timer (so called T2 in RFC 2131).
    pub timer_rebinding: Option<Forthon>,
    /// Lease expiration timer.
    pub timer_expiration: Option<Forthon>,
}

impl State {
    /// Constructs a default state.
    pub fn new(
        dhcp_state: DhcpState,
        server_address: Option<Ipv4Addr>,
        is_broadcast: bool,
    ) -> Self {
        State {
            dhcp_state,
            is_broadcast,
            transaction_id: rand::random::<u32>(),
            offered_address: Ipv4Addr::new(0, 0, 0, 0),
            offered_time: 0u32,
            dhcp_server_id: server_address,
            assigned_address: Ipv4Addr::new(0, 0, 0, 0),

            requested_at: 0i64,
            renewal_after: 0u64,
            rebinding_after: 0u64,
            expiration_after: 0u64,

            timer_offer: None,
            timer_ack: None,
            timer_renewal: None,
            timer_rebinding: None,
            timer_expiration: None,
        }
    }

    /// Moves the client from one state to another.
    ///
    /// # Panics
    /// On an unexpected state transcension.
    pub fn transcend(&mut self, from: DhcpState, to: DhcpState, response: Option<&Message>) {
        use self::DhcpState::*;
        trace!("Transcending from {} to {}", from, to);

        match from {
            Init => match to {
                next @ Selecting => {
                    self.set_dhcp_server_id(None);
                    self.run_timer_offer();
                    self.dhcp_state = next;
                }
                _ => panic_state!(from, to),
            },
            Selecting => match to {
                next @ SelectingSent => {
                    self.dhcp_state = next;
                }
                _ => panic_state!(from, to),
            },
            SelectingSent => match to {
                next @ Selecting => self.dhcp_state = next,
                next @ Requesting => {
                    let offer = expect!(response);
                    self.set_dhcp_server_id(Some(expect!(offer.options.dhcp_server_id)));
                    self.set_offered_address(offer.your_ip_address);
                    self.set_offered_time(expect!(offer.options.address_time));
                    self.run_timer_ack();
                    self.dhcp_state = next;
                }
                _ => panic_state!(from, to),
            },
            Requesting => match to {
                next @ RequestingSent => {
                    self.record_request_time();
                    self.dhcp_state = next;
                }
                _ => panic_state!(from, to),
            },
            RequestingSent => match to {
                next @ Init => self.dhcp_state = next,
                next @ Requesting => self.dhcp_state = next,
                next @ Bound => {
                    let ack = expect!(response);
                    self.set_assigned_address(ack.your_ip_address);
                    self.set_times(
                        ack.options.renewal_time,
                        ack.options.rebinding_time,
                        expect!(ack.options.address_time),
                    );
                    self.run_timer_renewal();
                    self.dhcp_state = next;
                }
                _ => panic_state!(from, to),
            },

            InitReboot => match to {
                next @ Rebooting => {
                    self.run_timer_ack();
                    self.dhcp_state = next;
                }
                _ => panic_state!(from, to),
            },
            Rebooting => match to {
                next @ RebootingSent => {
                    self.record_request_time();
                    self.dhcp_state = next;
                }
                _ => panic_state!(from, to),
            },
            RebootingSent => match to {
                next @ Init => self.dhcp_state = next,
                next @ Rebooting => self.dhcp_state = next,
                next @ Bound => {
                    let ack = expect!(response);
                    self.set_assigned_address(ack.your_ip_address);
                    self.set_dhcp_server_id(Some(expect!(ack.options.dhcp_server_id)));
                    self.set_times(
                        ack.options.renewal_time,
                        ack.options.rebinding_time,
                        expect!(ack.options.address_time),
                    );
                    self.run_timer_renewal();
                    self.dhcp_state = next;
                }
                _ => panic_state!(from, to),
            },

            Bound => match to {
                next @ Renewing => {
                    self.run_timer_rebinding();
                    self.dhcp_state = next;
                }
                _ => panic_state!(from, to),
            },
            Renewing => match to {
                next @ RenewingSent => {
                    self.record_request_time();
                    self.dhcp_state = next;
                }
                _ => panic_state!(from, to),
            },
            RenewingSent => match to {
                next @ Bound => {
                    let ack = expect!(response);
                    self.set_assigned_address(ack.your_ip_address);
                    self.set_dhcp_server_id(Some(expect!(ack.options.dhcp_server_id)));
                    self.set_times(
                        ack.options.renewal_time,
                        ack.options.rebinding_time,
                        expect!(ack.options.address_time),
                    );
                    self.run_timer_renewal();
                    self.dhcp_state = next;
                }
                next @ Renewing => self.dhcp_state = next,
                next @ Rebinding => {
                    self.set_dhcp_server_id(None);
                    self.run_timer_expiration();
                    self.dhcp_state = next;
                }
                _ => panic_state!(from, to),
            },
            Rebinding => match to {
                next @ RebindingSent => {
                    self.record_request_time();
                    self.dhcp_state = next;
                }
                _ => panic_state!(from, to),
            },
            RebindingSent => match to {
                next @ Init => self.dhcp_state = next,
                next @ Bound => {
                    let ack = expect!(response);
                    self.set_assigned_address(ack.your_ip_address);
                    self.set_dhcp_server_id(Some(expect!(ack.options.dhcp_server_id)));
                    self.set_times(
                        ack.options.renewal_time,
                        ack.options.rebinding_time,
                        expect!(ack.options.address_time),
                    );
                    self.run_timer_renewal();
                    self.dhcp_state = next;
                }
                next @ Rebinding => self.dhcp_state = next,
                _ => panic_state!(from, to),
            },
        }
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

    #[allow(dead_code)]
    fn set_broadcast(&mut self, value: bool) {
        self.is_broadcast = value;
    }

    fn set_offered_address(&mut self, value: Ipv4Addr) {
        self.offered_address = value;
    }

    fn set_offered_time(&mut self, value: u32) {
        self.offered_time = value;
    }

    fn set_dhcp_server_id(&mut self, value: Option<Ipv4Addr>) {
        self.dhcp_server_id = value;
    }

    fn set_assigned_address(&mut self, value: Ipv4Addr) {
        self.assigned_address = value;
    }

    fn record_request_time(&mut self) {
        self.requested_at = Utc::now().timestamp();
    }

    fn set_times(
        &mut self,
        renewal_time: Option<u32>,
        rebinding_time: Option<u32>,
        expiration_time: u32,
    ) {
        let renewal_time =
            renewal_time.unwrap_or(((expiration_time as f64) * RENEWAL_TIME_FACTOR) as u32);
        let rebinding_time =
            rebinding_time.unwrap_or(((expiration_time as f64) * REBINDING_TIME_FACTOR) as u32);

        self.renewal_after =
            ((renewal_time as i64) - (Utc::now().timestamp() - self.requested_at)) as u64;
        self.rebinding_after = (rebinding_time as u64) - self.renewal_after;
        self.expiration_after =
            (expiration_time as u64) - self.renewal_after - self.rebinding_after;
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
            Instant::now() + Duration::from_secs(self.renewal_after),
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
