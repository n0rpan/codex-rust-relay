//! The direction of an RPC frame depends on both the method and the presence of an ID.
//! Client and server request IDs occupy independent namespaces.
use super::activity::Activity;
use super::{
    names::{Method, Notice, ServerMethod},
    reply::Reply,
};
use serde::Deserialize;
use serde_json::{Value, from_value};
use std::io;

pub use crate::runtime::RequestId;

#[derive(Debug, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

#[derive(Debug)]
pub enum Frame {
    Activity(Activity),
    InvalidNotice {
        method: Notice,
        reason: String,
    },
    Reply {
        id: RequestId,
        result: Result<Reply, RpcError>,
    },
    Ask(super::ask::Ask),
    Notice(super::notice::Notification),
    Unknown {
        method: String,
        id: Option<RequestId>,
        params: Value,
    },
}

impl Frame {
    pub fn decode(
        line: &[u8],
        lookup: impl FnOnce(&RequestId) -> Option<Method>,
    ) -> io::Result<Self> {
        let mut object: serde_json::Map<String, Value> = serde_json::from_slice(line)?;
        let id = object
            .remove("id")
            .map(from_value::<RequestId>)
            .transpose()?;
        if let Some(method) = object.remove("method") {
            let method: String = from_value(method)?;
            let params = object.remove("params").unwrap_or(Value::Null);
            return match id {
                Some(id) => Ok(match ServerMethod::parse(&method) {
                    Some(method) => Self::Ask(super::ask::Ask::decode(id, method, params)),
                    None => Self::Unknown {
                        id: Some(id),
                        method,
                        params,
                    },
                }),
                None => Ok(match Notice::parse(&method) {
                    Some(method) if Activity::handles(method) => {
                        match Activity::decode(method, params) {
                            Ok(activity) => Self::Activity(activity),
                            Err(error) => Self::InvalidNotice {
                                method,
                                reason: error.to_string(),
                            },
                        }
                    }
                    Some(method) => match super::notice::Notification::decode(method, params) {
                        Ok(notice) => Self::Notice(notice),
                        Err(error) => Self::InvalidNotice {
                            method,
                            reason: error.to_string(),
                        },
                    },
                    None => Self::Unknown {
                        id: None,
                        method,
                        params,
                    },
                }),
            };
        }
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid Codex RPC reply");
        let id = id.ok_or_else(invalid)?;
        let method = lookup(&id);
        let result = match (object.remove("result"), object.remove("error")) {
            (Some(value), None) => Reply::decode(method, value).map_err(|err| RpcError {
                code: -32603,
                message: format!(
                    "Invalid response for {}: {err}",
                    method.map_or("unknown request", Method::as_str)
                ),
                data: None,
            }),
            (None, Some(error)) => Err(from_value(error)?),
            _ => return Err(invalid()),
        };
        Ok(Self::Reply { id, result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_requests_keep_the_exact_reply_id() {
        let frame =
            Frame::decode(br#"{"id":"2","method":"future/ask","params":{}}"#, |_| None).unwrap();
        assert!(
            matches!(frame, Frame::Unknown { id: Some(RequestId::Text(ref s)), .. } if s == "2")
        );
        let reply = Frame::decode(br#"{"id":2,"result":null}"#, |_| None).unwrap();
        assert!(matches!(
            reply,
            Frame::Reply {
                id: RequestId::Number(2),
                result: Ok(Reply::Unknown(Value::Null))
            }
        ));
        assert!(Frame::decode(br#"{"id":2}"#, |_| None).is_err());
    }
}
