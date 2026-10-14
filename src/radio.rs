//! Windows 原生 WLAN 扫描传输；不加入开发板网络，也不启用本机热点。
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::WiFi::*;
use windows::core::GUID;

use crate::events::Device;
use crate::radio_protocol::{self, Response};

pub const INTERVAL: Duration = Duration::from_secs(15);
type Notice = (GUID, bool);

fn check(code: u32, operation: &str) -> Result<(), String> {
    if code == 0 {
        return Ok(());
    }
    let hint = match code {
        5 => "；请检查 Windows 定位权限（Wi-Fi 扫描需要）",
        1062 => "；请启动 WLAN AutoConfig 服务",
        5023 => "；请检查无线网卡是否启用、飞行模式是否关闭",
        _ => "",
    };
    Err(format!("{operation}失败，Windows 错误码 {code}{hint}"))
}

unsafe extern "system" fn notification(data: *mut L2_NOTIFICATION_DATA, context: *mut c_void) {
    if data.is_null() || context.is_null() {
        return;
    }
    // 上下文保持有效，直到注销通知并关闭句柄。
    let data = unsafe { &*data };
    if data.NotificationSource != WLAN_NOTIFICATION_SOURCE_ACM {
        return;
    }
    if data.NotificationCode == wlan_notification_acm_scan_complete.0 as u32
        || data.NotificationCode == wlan_notification_acm_scan_fail.0 as u32
    {
        let tx = unsafe { &*(context as *const SyncSender<Notice>) };
        let _ = tx.try_send((
            data.InterfaceGuid,
            data.NotificationCode == wlan_notification_acm_scan_complete.0 as u32,
        ));
    }
}

struct Memory(*mut c_void);
impl Drop for Memory {
    fn drop(&mut self) {
        unsafe { WlanFreeMemory(self.0) };
    }
}

struct Session {
    handle: HANDLE,
    // 原生回调使用稳定的堆地址，避免结构体移动导致上下文地址失效。
    sender: Box<SyncSender<Notice>>,
    receiver: Receiver<Notice>,
}
impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            WlanRegisterNotification(
                self.handle,
                WLAN_NOTIFICATION_SOURCE_NONE,
                false,
                None,
                None,
                None,
                None,
            );
            WlanCloseHandle(self.handle, None);
        }
    }
}

#[repr(C)]
struct ScanIe {
    size: u32,
    data: [u8; 64],
}

impl Session {
    fn open() -> Result<Self, String> {
        let mut handle = HANDLE::default();
        let mut version = 0;
        check(
            unsafe { WlanOpenHandle(2, None, &mut version, &mut handle) },
            "打开无线接口",
        )?;
        let (tx, rx) = mpsc::sync_channel(16);
        let session = Self {
            handle,
            sender: Box::new(tx),
            receiver: rx,
        };
        check(
            unsafe {
                WlanRegisterNotification(
                    handle,
                    WLAN_NOTIFICATION_SOURCE_ACM,
                    false,
                    Some(notification),
                    Some((&*session.sender as *const SyncSender<Notice>).cast()),
                    None,
                    None,
                )
            },
            "注册扫描通知",
        )?;
        Ok(session)
    }

    fn interfaces(&self) -> Result<Vec<GUID>, String> {
        let mut list = std::ptr::null_mut();
        check(
            unsafe { WlanEnumInterfaces(self.handle, None, &mut list) },
            "枚举无线网卡",
        )?;
        if list.is_null() {
            return Err("无线网卡列表为空".into());
        }
        let _memory = Memory(list.cast());
        let count = unsafe { (*list).dwNumberOfItems as usize };
        if count > 256 {
            return Err("无线网卡列表长度异常".into());
        }
        let items = unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!((*list).InterfaceInfo).cast::<WLAN_INTERFACE_INFO>(),
                count,
            )
        };
        let interfaces = items.iter().map(|i| i.InterfaceGuid).collect::<Vec<_>>();
        if interfaces.is_empty() {
            return Err("没有无线网卡，无法扫描开发板广播".into());
        }
        Ok(interfaces)
    }

    fn scan(&self, interface: &GUID, ie: &[u8]) -> Result<(), String> {
        let mut raw = ScanIe {
            size: ie.len() as u32,
            data: [0; 64],
        };
        if ie.len() > raw.data.len() {
            return Err("扫描挑战报文过长".into());
        }
        raw.data[..ie.len()].copy_from_slice(ie);
        while self.receiver.try_recv().is_ok() {}
        check(
            unsafe {
                WlanScan(
                    self.handle,
                    interface,
                    None,
                    Some((&raw as *const ScanIe).cast::<WLAN_RAW_DATA>()),
                    None,
                )
            },
            "发送无线扫描挑战",
        )?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self
                .receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok((guid, true)) if guid == *interface => return Ok(()),
                Ok((guid, false)) if guid == *interface => {
                    return Err("无线网卡报告扫描失败".into());
                }
                Ok(_) => continue,
                Err(_) => return Err("等待无线扫描完成超时".into()),
            }
        }
    }

    fn replies(
        &self,
        interface: &GUID,
        nonce: &[u8; 16],
        since: u64,
    ) -> Result<Vec<Sighting>, String> {
        let mut list = std::ptr::null_mut();
        check(
            unsafe {
                WlanGetNetworkBssList(
                    self.handle,
                    interface,
                    None,
                    dot11_BSS_type_any,
                    false,
                    None,
                    &mut list,
                )
            },
            "读取无线广播",
        )?;
        if list.is_null() {
            return Err("无线广播列表为空".into());
        }
        let _memory = Memory(list.cast());
        let total = unsafe { (*list).dwTotalSize as usize };
        let count = unsafe { (*list).dwNumberOfItems as usize };
        let header = std::mem::offset_of!(WLAN_BSS_LIST, wlanBssEntries);
        let entry_size = std::mem::size_of::<WLAN_BSS_ENTRY>();
        if total < header || total > 16 * 1024 * 1024 || count > (total - header) / entry_size {
            return Err("无线广播列表边界异常".into());
        }
        let bytes = unsafe { std::slice::from_raw_parts(list.cast::<u8>(), total) };
        let mut replies = Vec::new();
        let mut fresh = 0;
        let mut vendor_bytes = 0;
        for index in 0..count {
            let offset = header + index * entry_size;
            let entry = unsafe {
                std::ptr::read_unaligned(bytes.as_ptr().add(offset).cast::<WLAN_BSS_ENTRY>())
            };
            // 本轮挑战之前接收的缓存广播不能用于判断在线。
            if entry.ullHostTimestamp < since {
                continue;
            }
            fresh += 1;
            let Some(ies) = ie_slice(
                bytes,
                offset,
                entry.ulIeOffset as usize,
                entry.ulIeSize as usize,
            ) else {
                continue;
            };
            vendor_bytes += ies.len();
            for reply in radio_protocol::responses(ies, nonce) {
                replies.push(Sighting {
                    reply,
                    rssi: entry.lRssi,
                });
            }
        }
        crate::log_info!(
            "无线扫描完成: bss={count} fresh={fresh} ie_bytes={vendor_bytes} authenticated={}",
            replies.len()
        );
        Ok(replies)
    }
}

fn ie_slice(bytes: &[u8], entry: usize, offset: usize, len: usize) -> Option<&[u8]> {
    if offset < std::mem::size_of::<WLAN_BSS_ENTRY>() {
        return None;
    }
    let begin = entry.checked_add(offset)?;
    bytes.get(begin..begin.checked_add(len)?)
}

struct Sighting {
    reply: Response,
    rssi: i32,
}

fn filetime_now() -> u64 {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    116_444_736_000_000_000 + unix.as_nanos() as u64 / 100
}

/// 两次扫描使用同一挑战：第一次发送请求，第二次可接收固件主循环发布的新应答。
fn exchange(target: Option<u32>) -> Result<Vec<Sighting>, String> {
    let session = Session::open()?;
    let interfaces = session.interfaces()?;
    let mut nonce = [0; 16];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut nonce);
    let ie = radio_protocol::vendor_ie(&radio_protocol::challenge(&nonce));
    let since = filetime_now();
    let mut found = BTreeMap::new();
    let mut successful_scans = 0;
    let mut errors = Vec::new();
    for interface in interfaces {
        for _ in 0..2 {
            if let Err(error) = session.scan(&interface, &ie) {
                errors.push(error);
                break;
            }
            match session.replies(&interface, &nonce, since) {
                Ok(replies) => {
                    successful_scans += 1;
                    for sighting in replies {
                        if target.is_none_or(|id| id == sighting.reply.id) {
                            found.insert(sighting.reply.id, sighting);
                        }
                    }
                }
                Err(error) => {
                    errors.push(error);
                    break;
                }
            }
            if !found.is_empty() {
                break;
            }
        }
        if target.is_some() && !found.is_empty() {
            break;
        }
    }
    if successful_scans == 0 {
        return Err(errors.join("；"));
    }
    for error in errors {
        crate::log_warn!("部分无线接口扫描失败: {error}");
    }
    Ok(found.into_values().collect())
}

pub fn discover() -> Result<Vec<Device>, String> {
    Ok(exchange(None)?
        .into_iter()
        .map(|s| Device {
            name: format!("heartbeat-radio（{} dBm）", s.rssi),
            addr: format!("radio://{:06x}", s.reply.id),
        })
        .collect())
}

pub fn heartbeat(target: &str) -> Result<Duration, String> {
    let id = radio_protocol::target_id(target)?;
    let start = Instant::now();
    let sighting = exchange(Some(id))?
        .into_iter()
        .next()
        .ok_or("扫描完成，未收到本轮挑战的有效广播应答（未使用缓存或旧应答）")?;
    let r = sighting.reply;
    crate::logger::write_esp(&format!(
        "radio id={id:06x} uptime={}s boot={:016x} seq={} rssi={} heap={} min={} maxblock={} frag={} reset={} cause={} pc={:08x} address={:08x} rx={} bad={}",
        r.uptime,
        r.boot,
        r.sequence,
        sighting.rssi,
        r.heap,
        r.min_heap,
        r.max_block,
        r.fragmentation,
        r.reset_reason,
        r.crash_cause,
        r.crash_pc,
        r.crash_address,
        r.received,
        r.invalid
    ));
    Ok(start.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_checks_untrusted_ie_offsets() {
        let bytes = vec![0u8; 512];
        assert!(ie_slice(&bytes, 8, usize::MAX, 1).is_none());
        assert!(ie_slice(&bytes, 8, 400, usize::MAX).is_none());
        assert!(ie_slice(&bytes, 8, 1, 32).is_none());
        assert!(ie_slice(&bytes, 8, 400, 105).is_none());
        assert_eq!(ie_slice(&bytes, 8, 400, 100).unwrap().len(), 100);
    }
}
