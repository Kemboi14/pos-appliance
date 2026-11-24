//! Wi-Fi through the firmware's own Wi-Fi driver (UEFI 2.5+):
//!
//! * `EFI_WIRELESS_MAC_CONNECTION_II_PROTOCOL` on the Wi-Fi NIC handle
//!   scans and joins networks;
//! * `EFI_SUPPLICANT_PROTOCOL` (on the NIC handle, or a child made with
//!   its service binding) takes the network name and WPA2/WPA3-Personal
//!   passphrase before joining, as EDK2's Wi-Fi Connection Manager does.
//!
//! Once joined, the same NIC handle's Simple Network Protocol carries
//! ordinary Ethernet frames, so the store network runs over it unchanged
//! (see `net::UefiNic::init`).
//!
//! Only firmware that ships a Wi-Fi driver has these protocols (some
//! business laptops and mini-PCs); elsewhere `present()` is false. QEMU has
//! no Wi-Fi device, so this file is only build- and lint-checked in CI and
//! must be tried on the real machine.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::{self, NonNull};
use core::time::Duration;

use pos_hal::{HalError, HalResult, WifiNetwork};
use uefi::boot::{self, EventType, ScopedProtocol, Tpl};
use uefi::proto::unsafe_protocol;
use uefi::{Handle, Status};

// ------------------------------------------------------------ raw types

#[repr(C)]
#[derive(Clone, Copy)]
struct Ssid {
    len: u8,
    ssid: [u8; 32],
}

impl Ssid {
    fn new(s: &str) -> Ssid {
        let mut ssid = [0u8; 32];
        let n = s.len().min(32);
        ssid[..n].copy_from_slice(&s.as_bytes()[..n]);
        Ssid { len: n as u8, ssid }
    }
    fn text(&self) -> String {
        let n = usize::from(self.len).min(32);
        String::from_utf8_lossy(&self.ssid[..n]).into_owned()
    }
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Suite {
    oui: [u8; 3],
    kind: u8,
}

/// `EFI_80211_AKM_SUITE_SELECTOR` / `EFI_80211_CIPHER_SUITE_SELECTOR`:
/// a count followed by that many suites.
#[repr(C)]
struct SuiteList {
    count: u16,
    list: [Suite; 1],
}

#[repr(C)]
struct Network {
    bss_type: u32,
    ssid: Ssid,
    akm: *const SuiteList,
    cipher: *const SuiteList,
}

#[repr(C)]
struct NetworkDesc {
    network: Network,
    quality: u8,
}

#[repr(C)]
struct GetNetworksData {
    num_ssid: u32,
    ssid_list: [Ssid; 1],
}

#[repr(C)]
struct GetNetworksResult {
    num: u8,
    desc: [NetworkDesc; 1],
}

#[repr(C)]
struct GetNetworksToken {
    event: *mut c_void,
    status: Status,
    data: *mut GetNetworksData,
    result: *mut GetNetworksResult,
}

#[repr(C)]
struct ConnectData {
    network: *mut Network,
    failure_timeout: u32,
}

#[repr(C)]
struct ConnectToken {
    event: *mut c_void,
    status: Status,
    data: *mut ConnectData,
    result_code: u32,
}

#[repr(C)]
struct DisconnectToken {
    event: *mut c_void,
    status: Status,
}

#[unsafe_protocol("1b0fb9bf-699d-4fdd-a7c3-2546681bf63b")]
#[repr(C)]
struct WirelessMacConnectionII {
    get_networks:
        unsafe extern "efiapi" fn(*mut WirelessMacConnectionII, *mut GetNetworksToken) -> Status,
    connect_network:
        unsafe extern "efiapi" fn(*mut WirelessMacConnectionII, *mut ConnectToken) -> Status,
    disconnect_network:
        unsafe extern "efiapi" fn(*mut WirelessMacConnectionII, *mut DisconnectToken) -> Status,
}

#[unsafe_protocol("54fcc43e-aa89-4333-9a85-cdea24051e9e")]
#[repr(C)]
struct Supplicant {
    build_response_packet: usize,
    process_packet: usize,
    set_data: unsafe extern "efiapi" fn(*mut Supplicant, u32, *const c_void, usize) -> Status,
    get_data: usize,
}

#[unsafe_protocol("45bcd98e-59ad-4174-9546-344a07485898")]
#[repr(C)]
struct SupplicantServiceBinding {
    create_child:
        unsafe extern "efiapi" fn(*mut SupplicantServiceBinding, *mut *mut c_void) -> Status,
    destroy_child: unsafe extern "efiapi" fn(*mut SupplicantServiceBinding, *mut c_void) -> Status,
}

/// `EFI_SUPPLICANT_DATA_TYPE` values used here.
const SUP_PSK_PASSWORD: u32 = 3;
const SUP_TARGET_SSID: u32 = 4;

/// IEEE 802.11 OUI and suite types.
const OUI: [u8; 3] = [0x00, 0x0F, 0xAC];
const AKM_PSK: Suite = Suite { oui: OUI, kind: 2 };
const AKM_SAE: Suite = Suite { oui: OUI, kind: 8 };
const CIPHER_CCMP: Suite = Suite { oui: OUI, kind: 4 };

const INFRASTRUCTURE_BSS: u32 = 0;

// ------------------------------------------------------------ the adapter

/// What a scan said about a network, kept to join it later.
struct Known {
    ssid: String,
    akm: Vec<Suite>,
    cipher: Vec<Suite>,
}

pub struct UefiWifi {
    wmc: Option<ScopedProtocol<WirelessMacConnectionII>>,
    supplicant: Option<ScopedProtocol<Supplicant>>,
    known: Vec<Known>,
    joined: Option<String>,
}

impl core::fmt::Debug for UefiWifi {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UefiWifi")
            .field("present", &self.wmc.is_some())
            .field("joined", &self.joined)
            .finish()
    }
}

fn shared<P: uefi::proto::ProtocolPointer + ?Sized>(h: Handle) -> Option<ScopedProtocol<P>> {
    crate::open_shared::<P>(h)
}

/// A firmware event we poll (no notify function).
struct Waiter(Option<uefi::Event>);

impl Waiter {
    fn new() -> HalResult<Waiter> {
        // SAFETY: no notify function or context.
        unsafe { boot::create_event(EventType::empty(), Tpl::CALLBACK, None, None) }
            .map(|e| Waiter(Some(e)))
            .map_err(|_| HalError::Io)
    }
    fn ptr(&self) -> *mut c_void {
        self.0.as_ref().map_or(ptr::null_mut(), uefi::Event::as_ptr)
    }
    /// Wait until the driver signals the token, or time out.
    fn wait(&self, limit: Duration) -> HalResult<()> {
        let Some(e) = &self.0 else {
            return Err(HalError::Io);
        };
        let step = Duration::from_millis(50);
        let mut waited = Duration::ZERO;
        while waited < limit {
            if boot::check_event(e).unwrap_or(false) {
                return Ok(());
            }
            boot::stall(step);
            waited += step;
        }
        Err(HalError::Timeout)
    }
    /// The driver may still signal it: never close it.
    fn abandon(mut self) {
        let _ = self.0.take();
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        if let Some(e) = self.0.take() {
            let _ = boot::close_event(e);
        }
    }
}

/// Wait for a request; on timeout leak its buffers and event, since the
/// driver may still write to them later.
fn finish<T>(
    ev: Waiter,
    req: alloc::boxed::Box<T>,
    limit: Duration,
) -> HalResult<alloc::boxed::Box<T>> {
    match ev.wait(limit) {
        Ok(()) => Ok(req),
        Err(e) => {
            alloc::boxed::Box::leak(req);
            ev.abandon();
            Err(e)
        }
    }
}

fn status_err(s: Status) -> HalError {
    match s {
        Status::NOT_FOUND => HalError::NotFound,
        Status::TIMEOUT => HalError::Timeout,
        Status::ACCESS_DENIED | Status::SECURITY_VIOLATION => HalError::Refused,
        Status::UNSUPPORTED => HalError::NotPresent,
        _ => HalError::Io,
    }
}

/// Copy a driver-owned suite list (count + entries) into a Vec.
///
/// # Safety
/// `p` is null or points to a valid suite list.
unsafe fn suites(p: *const SuiteList) -> Vec<Suite> {
    if p.is_null() {
        return Vec::new();
    }
    // SAFETY: per the caller; entries follow the count contiguously.
    unsafe {
        let n = usize::from(ptr::addr_of!((*p).count).read_unaligned());
        let first = ptr::addr_of!((*p).list).cast::<Suite>();
        (0..n.min(16))
            .map(|i| first.add(i).read_unaligned())
            .collect()
    }
}

/// An owned suite list laid out like the C struct.
fn suite_buf(v: &[Suite]) -> Vec<u8> {
    let mut b = vec![0u8; 4 + 4 * v.len().max(1)];
    b[..2].copy_from_slice(&(v.len() as u16).to_le_bytes());
    for (i, s) in v.iter().enumerate() {
        // `list` starts at offset 2 (Suite is byte-aligned).
        let o = 2 + 4 * i;
        b[o..o + 3].copy_from_slice(&s.oui);
        b[o + 3] = s.kind;
    }
    b
}

impl UefiWifi {
    /// Find a Wi-Fi NIC with a firmware driver, and its supplicant.
    pub fn init() -> UefiWifi {
        let mut me = UefiWifi {
            wmc: None,
            supplicant: None,
            known: Vec::new(),
            joined: None,
        };
        let find = || {
            boot::find_handles::<WirelessMacConnectionII>()
                .ok()
                .and_then(|v| v.first().copied())
        };
        let found = find().or_else(|| {
            // Boot managers often connect only the boot device: bind
            // drivers to every device once, then look again.
            if let Ok(hs) = boot::find_handles::<uefi::proto::device_path::DevicePath>() {
                for h in hs {
                    let _ = boot::connect_controller(h, &[], None, true);
                }
            }
            find()
        });
        let Some(h) = found else {
            return me;
        };
        me.wmc = shared::<WirelessMacConnectionII>(h);
        me.supplicant = shared::<Supplicant>(h).or_else(|| {
            let mut sb = shared::<SupplicantServiceBinding>(h)?;
            let mut child: *mut c_void = ptr::null_mut();
            let sbp: *mut SupplicantServiceBinding = &mut *sb;
            // SAFETY: valid protocol pointer; `child` receives a handle.
            let st = unsafe { ((*sbp).create_child)(sbp, &mut child) };
            if st.is_error() {
                return None;
            }
            // SAFETY: the firmware just created this handle.
            let ch = unsafe { Handle::from_ptr(child) }?;
            shared::<Supplicant>(ch)
        });
        me
    }

    /// The handle the store network should use for frames, if any.
    pub fn nic_handles() -> Vec<Handle> {
        boot::find_handles::<WirelessMacConnectionII>().unwrap_or_default()
    }

    fn wmc(&mut self) -> HalResult<*mut WirelessMacConnectionII> {
        match &mut self.wmc {
            Some(p) => Ok(&mut **p),
            None => Err(HalError::NotPresent),
        }
    }

    fn set_credentials(&mut self, ssid: &str, password: &str) -> HalResult<()> {
        let Some(sup) = &mut self.supplicant else {
            // Some drivers carry their own supplicant; let them try.
            return Ok(());
        };
        let sp: *mut Supplicant = &mut **sup;
        let s = Ssid::new(ssid);
        let mut pw: Vec<u8> = password.as_bytes().to_vec();
        pw.push(0);
        // SAFETY: valid protocol; buffers outlive the calls (copied by the
        // supplicant).
        unsafe {
            let st = ((*sp).set_data)(
                sp,
                SUP_TARGET_SSID,
                ptr::addr_of!(s).cast(),
                core::mem::size_of::<Ssid>(),
            );
            if st.is_error() {
                return Err(status_err(st));
            }
            let st = ((*sp).set_data)(sp, SUP_PSK_PASSWORD, pw.as_ptr().cast(), pw.len());
            if st.is_error() {
                return Err(status_err(st));
            }
        }
        pw.fill(0);
        Ok(())
    }
}

impl pos_hal::Wifi for UefiWifi {
    fn present(&self) -> bool {
        self.wmc.is_some()
    }

    fn describe(&self) -> String {
        match (&self.wmc, &self.supplicant) {
            (None, _) => String::from("no firmware Wi-Fi driver (UEFI)"),
            (Some(_), Some(_)) => String::from("firmware Wi-Fi driver, WPA supplicant"),
            (Some(_), None) => String::from("firmware Wi-Fi driver, no separate supplicant"),
        }
    }

    fn scan(&mut self) -> HalResult<Vec<WifiNetwork>> {
        let wmc = self.wmc()?;
        let ev = Waiter::new()?;
        struct Req {
            data: GetNetworksData,
            token: GetNetworksToken,
        }
        let mut req = alloc::boxed::Box::new(Req {
            data: GetNetworksData {
                num_ssid: 0,
                ssid_list: [Ssid::new("")],
            },
            token: GetNetworksToken {
                event: ev.ptr(),
                status: Status::SUCCESS,
                data: ptr::null_mut(),
                result: ptr::null_mut(),
            },
        });
        req.token.data = &mut req.data;
        // SAFETY: valid protocol; the boxed request outlives the call (it
        // is leaked if the driver never answers).
        let st = unsafe { ((*wmc).get_networks)(wmc, &mut req.token) };
        if st.is_error() {
            return Err(status_err(st));
        }
        let req = finish(ev, req, Duration::from_secs(15))?;
        // SAFETY: the driver wrote these fields before signalling.
        let (status, result) = unsafe {
            (
                ptr::addr_of!(req.token.status).read_volatile(),
                ptr::addr_of!(req.token.result).read_volatile(),
            )
        };
        if status.is_error() {
            return Err(status_err(status));
        }
        let mut out = Vec::new();
        self.known.clear();
        if !result.is_null() {
            // SAFETY: `result` is the driver's buffer: a count followed by
            // that many descriptions.
            unsafe {
                let n = usize::from(ptr::addr_of!((*result).num).read());
                let first = ptr::addr_of!((*result).desc).cast::<NetworkDesc>();
                for i in 0..n {
                    let d = first.add(i);
                    let net = ptr::addr_of!((*d).network);
                    if ptr::addr_of!((*net).bss_type).read() != INFRASTRUCTURE_BSS {
                        continue;
                    }
                    let ssid = ptr::addr_of!((*net).ssid).read().text();
                    let akm = suites(ptr::addr_of!((*net).akm).read());
                    let cipher = suites(ptr::addr_of!((*net).cipher).read());
                    let quality = ptr::addr_of!((*d).quality).read().min(100);
                    out.push(WifiNetwork {
                        ssid: ssid.clone(),
                        quality,
                        secured: akm.iter().any(|a| *a == AKM_PSK || *a == AKM_SAE),
                    });
                    self.known.push(Known { ssid, akm, cipher });
                }
                if let Some(p) = NonNull::new(result.cast::<u8>()) {
                    let _ = boot::free_pool(p);
                }
            }
        }
        Ok(out)
    }

    fn connect(&mut self, ssid: &str, password: &str) -> HalResult<()> {
        if self.joined.as_deref() == Some(ssid) {
            return Ok(());
        }
        if !self.known.iter().any(|k| k.ssid == ssid) {
            // Start-up: look for it first so we join it as advertised.
            let _ = pos_hal::Wifi::scan(self);
        }
        let (akm, cipher) = match self.known.iter().find(|k| k.ssid == ssid) {
            Some(k) => {
                // Prefer WPA2-PSK when offered (widest driver support).
                let akm = if k.akm.contains(&AKM_PSK) {
                    vec![AKM_PSK]
                } else if k.akm.contains(&AKM_SAE) {
                    vec![AKM_SAE]
                } else {
                    return Err(HalError::Refused);
                };
                let cipher = if k.cipher.contains(&CIPHER_CCMP) || k.cipher.is_empty() {
                    vec![CIPHER_CCMP]
                } else {
                    k.cipher.clone()
                };
                (akm, cipher)
            }
            None => (vec![AKM_PSK], vec![CIPHER_CCMP]),
        };
        self.set_credentials(ssid, password)?;
        let wmc = self.wmc()?;
        struct Req {
            akm: Vec<u8>,
            cipher: Vec<u8>,
            net: Network,
            data: ConnectData,
            token: ConnectToken,
        }
        let ev = Waiter::new()?;
        let mut req = alloc::boxed::Box::new(Req {
            akm: suite_buf(&akm),
            cipher: suite_buf(&cipher),
            net: Network {
                bss_type: INFRASTRUCTURE_BSS,
                ssid: Ssid::new(ssid),
                akm: ptr::null(),
                cipher: ptr::null(),
            },
            data: ConnectData {
                network: ptr::null_mut(),
                failure_timeout: 20,
            },
            token: ConnectToken {
                event: ev.ptr(),
                status: Status::SUCCESS,
                data: ptr::null_mut(),
                result_code: 0,
            },
        });
        req.net.akm = req.akm.as_ptr().cast();
        req.net.cipher = req.cipher.as_ptr().cast();
        req.data.network = &mut req.net;
        req.token.data = &mut req.data;
        // SAFETY: valid protocol; the boxed request outlives the call (it
        // is leaked if the driver never answers).
        let st = unsafe { ((*wmc).connect_network)(wmc, &mut req.token) };
        if st == Status::ALREADY_STARTED {
            self.joined = Some(ssid.into());
            return Ok(());
        }
        if st.is_error() {
            return Err(status_err(st));
        }
        let req = finish(ev, req, Duration::from_secs(25))?;
        // SAFETY: written by the driver before signalling.
        let (status, code) = unsafe {
            (
                ptr::addr_of!(req.token.status).read_volatile(),
                ptr::addr_of!(req.token.result_code).read_volatile(),
            )
        };
        if status.is_error() {
            return Err(status_err(status));
        }
        // EFI_80211_CONNECT_NETWORK_RESULT_CODE: 0 success, 1 refused,
        // 2 failed, 3 timeout, 4 unspecified.
        match code {
            0 => {
                self.joined = Some(ssid.into());
                Ok(())
            }
            1 | 2 => Err(HalError::Refused),
            3 => Err(HalError::Timeout),
            _ => Err(HalError::Io),
        }
    }

    fn disconnect(&mut self) -> HalResult<()> {
        let wmc = self.wmc()?;
        self.joined = None;
        let ev = Waiter::new()?;
        let mut token = alloc::boxed::Box::new(DisconnectToken {
            event: ev.ptr(),
            status: Status::SUCCESS,
        });
        // SAFETY: valid protocol; the boxed token outlives the call.
        let st = unsafe { ((*wmc).disconnect_network)(wmc, &mut *token) };
        if st.is_error() {
            return Err(status_err(st));
        }
        finish(ev, token, Duration::from_secs(5)).map(|_| ())
    }

    fn connected(&self) -> Option<String> {
        self.joined.clone()
    }
}
