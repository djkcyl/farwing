//! Power control and Wake-on-LAN.

use super::auth::{AccountAuth, AnyAuth};
use super::devices::{notify_changed, user_summary, visible_device};
use super::{App, ClientIp, JsonBody};
use crate::crypto::new_uuid;
use crate::db::{self, DeviceRow};
use crate::error::{ApiError, ApiResult};
use axum::Json;
use axum::extract::{Path, State};
use farwing_protocol::api::{
    Empty, PowerAction, PowerReceipt, PowerRequest, PowerResult, WolInfoRequest, WolInfoResponse,
};
use farwing_protocol::realtime::{
    DeviceChangeKind, Envelope, PowerRequestPush, WolTarget, WolWake, types,
};
use farwing_protocol::error::reason;
use farwing_protocol::{ErrorCode, caps, capability};
use std::net::{IpAddr, Ipv4Addr};

fn action_str(a: PowerAction) -> &'static str {
    match a {
        PowerAction::Wake => "wake",
        PowerAction::Reboot => "reboot",
        PowerAction::Shutdown => "shutdown",
    }
}

fn wol_target(row: &DeviceRow) -> WolTarget {
    WolTarget {
        mac: row.wol_mac.clone(),
        inner_ip: row.wol_inner_ip.clone(),
        subnet_mask: row.wol_subnet_mask.clone(),
    }
}

/// Whether two IPv4 LAN addresses are in the same subnet under `mask`
/// (`/24` when no mask is known).
fn same_subnet(a: &str, b: &str, mask: &str) -> bool {
    let (Ok(IpAddr::V4(a)), Ok(IpAddr::V4(b))) = (a.trim().parse(), b.trim().parse()) else {
        return false;
    };
    let mask = if mask.trim().is_empty() {
        Ipv4Addr::new(255, 255, 255, 0)
    } else {
        match mask.trim().parse::<Ipv4Addr>() {
            Ok(m) => m,
            Err(_) => return false,
        }
    };
    let m = u32::from(mask);
    u32::from(a) & m == u32::from(b) & m
}

/// The requester can send the magic packet itself: it reaches the server from
/// the target's public address and registered a LAN address in the target's
/// subnet.
fn requester_on_target_lan(requester: &DeviceRow, ip: IpAddr, target: &DeviceRow) -> bool {
    let mask = if target.wol_subnet_mask.is_empty() {
        &requester.wol_subnet_mask
    } else {
        &target.wol_subnet_mask
    };
    !target.wol_public_ip.is_empty()
        && ip.to_string() == target.wol_public_ip
        && !requester.wol_inner_ip.is_empty()
        && same_subnet(&requester.wol_inner_ip, &target.wol_inner_ip, mask)
}

/// `POST /api/v2/devices/{id}/power`: own devices, or friends' devices with
/// the `power` grant.
pub async fn request(
    State(app): State<App>,
    ClientIp(ip): ClientIp,
    auth: AccountAuth,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<PowerRequest>,
) -> ApiResult<Json<PowerReceipt>> {
    let visible = visible_device(&app, &auth, &id).await?;
    if !visible.granted.iter().any(|c| c == caps::POWER) {
        return Err(ApiError::reason(
            ErrorCode::Forbidden,
            reason::POWER_NOT_GRANTED,
            "the grants do not include power",
        ));
    }
    let target = visible.row;
    let rid = new_uuid();
    let record = {
        let (rid, did, uid, req_dev) = (
            rid.clone(),
            target.id.clone(),
            auth.user.id.clone(),
            auth.device.id.clone(),
        );
        let action = action_str(req.action);
        move |c: &mut rusqlite::Connection| {
            db::insert_power_request(c, &rid, &did, &uid, &req_dev, action)
        }
    };
    match req.action {
        PowerAction::Reboot | PowerAction::Shutdown => {
            if !app.hub.is_online(&target.id) {
                return Err(ApiError::device_offline());
            }
            if !target.has_capability(capability::POWER) || !target.policy.power {
                return Err(ApiError::reason(
                    ErrorCode::HostDisabled,
                    reason::POWER_DISABLED,
                    "host does not allow shutdown and restart",
                ));
            }
            app.db.call(record).await?;
            let pushed = app.hub.push(
                &target.id,
                &Envelope::new(
                    types::POWER_REQUEST,
                    PowerRequestPush {
                        request_id: rid.clone(),
                        action: req.action,
                        requester: user_summary(&auth.user),
                        requester_device: auth.device.alias.clone(),
                    },
                ),
            );
            if !pushed {
                return Err(ApiError::device_offline());
            }
            tracing::info!(request_id = %rid, device_id = %target.id, requester = %auth.user.id, action = action_str(req.action), "power request sent");
            power_event(&app, &auth, &target, visible.owner.as_ref(), req.action, ip).await;
            Ok(Json(PowerReceipt {
                request_id: rid,
                ..PowerReceipt::default()
            }))
        }
        PowerAction::Wake => {
            if app.hub.is_online(&target.id) {
                return Err(ApiError::reason(
                    ErrorCode::Conflict,
                    reason::DEVICE_ONLINE,
                    "target device is online",
                ));
            }
            if !target.wol_supported() {
                return Err(ApiError::reason(
                    ErrorCode::Forbidden,
                    reason::WOL_UNAVAILABLE,
                    "target device has no Wake-on-LAN set up",
                ));
            }
            app.db.call(record).await?;
            // Helpers: the owner's online devices behind the same public address.
            let owner = target.user_id.clone().unwrap_or_default();
            let devices = app.db.call(move |c| db::list_user_devices(c, &owner)).await?;
            let wake = Envelope::new(
                types::WOL_WAKE,
                WolWake {
                    request_id: rid.clone(),
                    target: wol_target(&target),
                },
            );
            let target_ip = target.wol_public_ip.clone();
            let mut helpers = 0u32;
            if !target_ip.is_empty() {
                for d in devices
                    .iter()
                    .filter(|d| d.id != target.id && d.id != auth.device.id)
                {
                    let helper_ip = if d.wol_public_ip.is_empty() {
                        app.hub.presence_ip(&d.id).map(|i| i.to_string())
                    } else {
                        Some(d.wol_public_ip.clone())
                    };
                    if helper_ip.as_deref() == Some(target_ip.as_str())
                        && app.hub.push(&d.id, &wake)
                    {
                        helpers += 1;
                    }
                }
            }
            power_event(&app, &auth, &target, visible.owner.as_ref(), req.action, ip).await;
            let same_network = requester_on_target_lan(&auth.device, ip, &target);
            tracing::info!(request_id = %rid, device_id = %target.id, helpers, same_network, "wake request routed");
            Ok(Json(PowerReceipt {
                request_id: rid,
                current_device_in_same_network: Some(same_network),
                assist_count: Some(helpers),
                target: same_network.then(|| wol_target(&target)),
            }))
        }
    }
}

/// An account event of the requester and of the device's owner.
async fn power_event(
    app: &App,
    auth: &AccountAuth,
    target: &DeviceRow,
    owner: Option<&db::UserRow>,
    action: PowerAction,
    ip: IpAddr,
) {
    let owner = owner.map_or(auth.user.username.as_str(), |o| o.username.as_str());
    let detail = serde_json::json!({
        "device": target.alias,
        "device_id": target.id,
        "action": action_str(action),
    });
    let actor = Some((auth.user.id.as_str(), auth.user.username.as_str()));
    super::me::audit(app, actor, "device.power", owner, detail, true, ip).await;
    // A friend did it: the owner hears of it.
    if target.user_id.as_deref().is_some_and(|o| o != auth.user.id) {
        let data = serde_json::json!({
            "device": target.alias,
            "who": auth.user.display_name(),
            "action": action_str(action),
        });
        crate::notify::send_owner(app, &target.id, farwing_protocol::api::notice::POWER_REMOTE, data, &[&target.id]);
    }
}

/// `POST /api/v2/power/{request_id}/result`: reported by the target device.
pub async fn result(
    State(app): State<App>,
    auth: AnyAuth,
    Path(rid): Path<String>,
    JsonBody(res): JsonBody<PowerResult>,
) -> ApiResult<Json<Empty>> {
    let id = rid.clone();
    let row = app
        .db
        .call(move |c| db::get_power_request(c, &id))
        .await?
        .ok_or_else(ApiError::not_found)?;
    if row.device_id != auth.device.id {
        return Err(ApiError::forbidden("only the target device reports the result"));
    }
    let (id, msg, state) = (row.id.clone(), res.message.clone(), res.state.clone());
    let code = res.code;
    app.db
        .call(move |c| db::set_power_result(c, &id, code, &msg, &state))
        .await?;
    tracing::info!(
        request_id = %row.id,
        device_id = %row.device_id,
        requester = %row.requester_device_id,
        action = %row.action,
        code = res.code,
        state = %db::clip(&res.state, 64),
        message = %db::clip(&res.message, 200),
        "power result"
    );
    Ok(Json(Empty {}))
}

fn normalize_mac(mac: &str) -> Option<String> {
    let hex: String = mac
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.' | ' '))
        .collect();
    (hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| {
        hex.as_bytes()
            .chunks(2)
            .map(|p| std::str::from_utf8(p).unwrap_or("00").to_ascii_uppercase())
            .collect::<Vec<_>>()
            .join(":")
    })
}

/// `PUT /api/v2/devices/self/wol`
pub async fn set_wol_info(
    State(app): State<App>,
    ClientIp(ip): ClientIp,
    auth: AccountAuth,
    JsonBody(req): JsonBody<WolInfoRequest>,
) -> ApiResult<Json<WolInfoResponse>> {
    let mac = normalize_mac(&req.mac);
    let inner_ok = req.inner_ip.trim().parse::<IpAddr>().is_ok();
    let mask_ok =
        req.subnet_mask.trim().is_empty() || req.subnet_mask.trim().parse::<IpAddr>().is_ok();
    let supported = mac.is_some() && inner_ok && mask_ok;
    let (id, public) = (auth.device.id.clone(), ip.to_string());
    let (mac_s, inner, mask) = if supported {
        (
            mac.unwrap_or_default(),
            req.inner_ip.trim().to_owned(),
            req.subnet_mask.trim().to_owned(),
        )
    } else {
        (String::new(), String::new(), String::new())
    };
    app.db
        .call(move |c| db::set_wol_info(c, &id, &mac_s, &inner, &mask, &public))
        .await?;
    if auth.device.policy.wol {
        notify_changed(&app, &auth.device.id, DeviceChangeKind::Updated, false).await;
    }
    Ok(Json(WolInfoResponse { supported }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn mac() {
        assert_eq!(
            super::normalize_mac("aa-bb-cc-dd-ee-ff").as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
        assert!(super::normalize_mac("zz").is_none());
    }

    #[test]
    fn subnets() {
        assert!(super::same_subnet("192.168.1.20", "192.168.1.30", "255.255.255.0"));
        assert!(!super::same_subnet("192.168.1.20", "192.168.2.30", "255.255.255.0"));
        assert!(super::same_subnet("10.1.2.3", "10.9.9.9", "255.0.0.0"));
        assert!(super::same_subnet("192.168.1.20", "192.168.1.30", ""));
        assert!(!super::same_subnet("fe80::1", "fe80::2", ""));
    }
}
