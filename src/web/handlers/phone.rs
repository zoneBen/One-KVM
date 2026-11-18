//! Phone target endpoints: global PC/phone switch, toolbar actions, video size.

use super::*;

use crate::phone::{PhoneAction, TargetMode, TargetStatus};

/// Current target + phone session status.
pub async fn target_status(State(state): State<Arc<AppState>>) -> Result<Json<TargetStatus>> {
    let target = state.stream_manager.target_mode().await;
    // While the phone owns the target, report the session status (Running/
    // Suspended/Error) instead of re-running adb probe, which would turn a
    // suspended session into a misleading "Ready" state.
    let phone = if target == TargetMode::Phone {
        state.phone.cached_status().await
    } else {
        state.phone.probe().await
    };
    Ok(Json(TargetStatus { target, phone }))
}

#[derive(Deserialize)]
pub struct SetTargetRequest {
    pub target: TargetMode,
}

/// Switch the global target. Existing WebRTC sessions are closed and must
/// reconnect; the frontend does that after the request returns.
pub async fn set_target(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SetTargetRequest>,
) -> Result<Json<TargetStatus>> {
    let status = state.stream_manager.set_target(request.target).await?;
    Ok(Json(status))
}

#[derive(Deserialize)]
pub struct PhoneActionRequest {
    pub action: PhoneAction,
}

/// Toolbar action (Back/Home/Recents/Power/Volume/Rotate/...).
pub async fn phone_action(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PhoneActionRequest>,
) -> Result<Json<LoginResponse>> {
    state.phone.send_action(request.action).await?;
    Ok(Json(LoginResponse {
        success: true,
        message: None,
    }))
}

#[derive(Deserialize)]
pub struct PhoneVideoSizeRequest {
    pub width: u32,
    pub height: u32,
}

/// The browser reports the decoded H.264 frame size so scrcpy's position
/// mapper receives the exact video dimensions (important after rotation).
pub async fn phone_video_size(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PhoneVideoSizeRequest>,
) -> Result<Json<LoginResponse>> {
    state
        .phone
        .set_video_size(request.width, request.height)
        .await?;
    Ok(Json(LoginResponse {
        success: true,
        message: None,
    }))
}
