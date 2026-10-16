use std::{ffi::OsString, os::unix::ffi::OsStringExt, process::ExitStatus};

use chromiumoxide::{
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
    #[error("Error during serialization or deserialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("Browser error: {0}")]
    Internal(CdpError),
    #[error("Request timed out.")]
    Timeout,
    #[error("FrameId {0:?} not found.")]
    FrameNotFound(FrameId),
    #[error("base64 decoding failed: {0}")]
    Base64Decode(#[from] base64::DecodeError),
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
    #[error("Failed to launch the browser: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to build config: {0}")]
    FailedToBuildConfig(String),
    #[error("No page with id `{0}`. Use list_pages to see the open pages.")]
    PageNotFound(String),
    #[error("Error: {0}")]
    Anyhow(#[from] anyhow::Error),
}

impl From<CdpError> for Error {
    fn from(value: CdpError) -> Self {
        match value {
            CdpError::Serde(error) => Error::Serde(error),
            CdpError::LaunchExit(s, e) => Error::LaunchExit(s, OsString::from_vec(e.into_vec())),
            CdpError::LaunchTimeout(e) => Error::LaunchTimeout(OsString::from_vec(e.into_vec())),
            CdpError::LaunchIo(error, stderr) => {
                Error::LaunchIo(error, OsString::from_vec(stderr.into_vec()))
            }
            CdpError::Timeout => Error::Timeout,
            CdpError::FrameNotFound(frame_id) => Error::FrameNotFound(frame_id),
            CdpError::DecodeError(decode_error) => Error::Base64Decode(decode_error),
            CdpError::ScrollingFailed(e) => Error::ScrollingFailed(e),
            CdpError::NotFound => Error::NotFound,
            CdpError::JavascriptException(exception_details) => {
                Error::JavascriptException(exception_details)
            }
            CdpError::Url(parse_error) => Error::ParseUrl(parse_error),
            other => Error::Internal(other),
        }
    }
}

impl IntoCallToolResult for Error {
    /// Every error is reported as a tool error (rather than a JSON-RPC error) so the agent can
    /// read it and recover.
    fn into_call_tool_result(self) -> Result<CallToolResult, ErrorData> {
        match self {
            Error::JavascriptException(exception_details) => {
                match serde_json::to_value(&exception_details) {
                    Ok(v) => Ok(CallToolResult::structured_error(v)),
                    Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
                }
            }
            other => Ok(CallToolResult::error(vec![ContentBlock::text(
                other.to_string(),
            )])),
        }
    }
}
