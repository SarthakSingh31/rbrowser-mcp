use std::{ffi::OsString, os::unix::ffi::OsStringExt, process::ExitStatus};

use chaser_oxide::{
    cdp::{browser_protocol::page::FrameId, js_protocol::runtime::ExceptionDetails},
    error::CdpError,
};
use rmcp::{
    ErrorData,
    handler::server::tool::IntoCallToolResult,
    model::{CallToolResult, ContentBlock},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Exception evaluating script: {0:?}")]
    JavascriptException(Box<ExceptionDetails>),
    #[error("Error during serialization or deserialization: {0:?}")]
    Serde(#[from] serde_json::Error),
    #[error("Internal error: {0}")]
    Internal(CdpError),
    #[error("Request timed out.")]
    Timeout,
    #[error("FrameId {0:?} not found.")]
    FrameNotFound(FrameId),
    #[error("base64 decoding failed {0}")]
    Base64DecodeError(#[from] base64::DecodeError),
    #[error("Scrolling failed: {0}")]
    ScrollingFailed(String),
    #[error("Requested value not found.")]
    NotFound,
    #[error("Failed to parse url: {0}")]
    ParseUrl(#[from] url::ParseError),
    #[error("Failed to launch browser with status: {0}\nStderr: {stderr}", stderr = .1.display())]
    LaunchExit(ExitStatus, OsString),
    #[error("Failed to launch and connect to the browser.\n Timeout while resolving websocket URL from browser process, stderr: {stderr}", stderr = .0.display())]
    LaunchTimeout(OsString),
    #[error(
        "Failed to launch and connect to the browser.\n Input/Output error while resolving websocket URL from browser process, stderr: {stderr}: {0}",
        stderr = .1.display()
    )]
    LaunchIo(std::io::Error, OsString),
    #[error("Failed to build config: {0}")]
    FailedToBuildConfig(String),
    #[error("Failed to find body node")]
    FailedToFindSelectedNode,
    #[error("Error: {0}")]
    AnyhowError(#[from] anyhow::Error),
}

impl From<CdpError> for Error {
    fn from(value: CdpError) -> Self {
        match value {
            CdpError::Ws(_)
            | CdpError::Io(_)
            | CdpError::InvalidMessage(_, _)
            | CdpError::Chrome(_)
            | CdpError::NoResponse
            | CdpError::UnexpectedWsMessage(_)
            | CdpError::ChromeMessage(_)
            | CdpError::ChannelSendError(_) => Error::Internal(value),
            CdpError::Serde(error) => Error::Serde(error),
            CdpError::LaunchExit(s, e) => Error::LaunchExit(s, OsString::from_vec(e.into_vec())),
            CdpError::LaunchTimeout(e) => Error::LaunchTimeout(OsString::from_vec(e.into_vec())),
            CdpError::LaunchIo(error, stderr) => {
                Error::LaunchIo(error, OsString::from_vec(stderr.into_vec()))
            }
            CdpError::Timeout => Error::Timeout,
            CdpError::FrameNotFound(frame_id) => Error::FrameNotFound(frame_id),
            CdpError::DecodeError(decode_error) => Error::Base64DecodeError(decode_error),
            CdpError::ScrollingFailed(e) => Error::ScrollingFailed(e),
            CdpError::NotFound => Error::NotFound,
            CdpError::JavascriptException(exception_details) => {
                Error::JavascriptException(exception_details)
            }
            CdpError::Url(parse_error) => Error::ParseUrl(parse_error),
        }
    }
}

impl IntoCallToolResult for Error {
    fn into_call_tool_result(self) -> Result<CallToolResult, ErrorData> {
        match self {
            Error::JavascriptException(exception_details) => {
                match serde_json::to_value(&exception_details) {
                    Ok(v) => Ok(CallToolResult::structured_error(v)),
                    Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
                }
            }
            Error::Serde(error) => Ok(CallToolResult::error(vec![
                ContentBlock::text("Failed to serialize or deserialize:"),
                ContentBlock::text(error.to_string()),
            ])),
            Error::Internal(cdp_error) => Err(ErrorData::internal_error(
                format!("Internal Error: {cdp_error:?}"),
                None,
            )),
            Error::Timeout => Ok(CallToolResult::error(vec![ContentBlock::text("Timeout")])),
            Error::FrameNotFound(frame_id) => Ok(CallToolResult::error(vec![ContentBlock::text(
                format!("Frame not found: {}", frame_id.inner()),
            )])),
            Error::Base64DecodeError(decode_error) => Ok(CallToolResult::error(vec![
                ContentBlock::text("Failed to decode base64:"),
                ContentBlock::text(decode_error.to_string()),
            ])),
            Error::ScrollingFailed(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                format!("Scrolling failed: {e}"),
            )])),
            Error::NotFound => Ok(CallToolResult::error(vec![ContentBlock::text("Not Found")])),
            Error::ParseUrl(parse_error) => Ok(CallToolResult::error(vec![ContentBlock::text(
                format!("Failed to parse url: {parse_error}"),
            )])),
            Error::LaunchExit(exit_status, os_string) => Ok(CallToolResult::error(vec![
                ContentBlock::text(format!(
                    "Failed to launch browser with status code: {exit_status}. Stderr output:"
                )),
                ContentBlock::text(os_string.to_string_lossy()),
            ])),
            Error::LaunchTimeout(os_string) => Ok(CallToolResult::error(vec![
                ContentBlock::text("Failed to launch the browser:"),
                ContentBlock::text(os_string.to_string_lossy()),
            ])),
            Error::LaunchIo(error, os_string) => Ok(CallToolResult::error(vec![
                ContentBlock::text(format!(
                    "Failed to launch the browser due to error: {error}. Stderr output:"
                )),
                ContentBlock::text(os_string.to_string_lossy()),
            ])),
            Error::FailedToBuildConfig(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                format!("Failed to build browser builder config: {e}"),
            )])),
            Error::FailedToFindSelectedNode => Ok(CallToolResult::error(vec![ContentBlock::text(
                "Failed to find the selected node",
            )])),
            Error::AnyhowError(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                e.to_string(),
            )])),
        }
    }
}
