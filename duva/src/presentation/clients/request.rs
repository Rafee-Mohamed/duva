use std::str::FromStr;

use crate::domains::{
    QueryIO,
    cluster_actors::{LazyOption, SessionRequest},
    operation_logs::WriteRequest,
    peers::identifier::{PeerIdentifier, TPeerAddress},
};
use anyhow::Context;
use chrono::{DateTime, Utc};

#[derive(Clone, Debug)]
pub enum ClientAction {
    Ping,
    Echo(String),
    Config { key: String, value: String },
    Get { key: String },
    MGet { keys: Vec<String> },
    IndexGet { key: String, index: u64 },
    Set { key: String, value: String },
    Append { key: String, value: String },
    SetWithExpiry { key: String, value: String, expiry: DateTime<Utc> },
    Keys { pattern: Option<String> },
    Delete { keys: Vec<String> },
    Save,
    Info,
    ClusterInfo,
    ClusterNodes,
    ClusterForget(PeerIdentifier),
    ClusterReshard,
    ReplicaOf(PeerIdentifier),
    Exists { keys: Vec<String> },
    Role,
    Incr { key: String },
    Decr { key: String },
    Ttl { key: String },
    ClusterMeet(PeerIdentifier, LazyOption),
    IncrBy { key: String, increment: i64 },
    DecrBy { key: String, decrement: i64 },
}

impl ClientAction {
    pub fn to_write_request(self) -> WriteRequest {
        match self {
            | ClientAction::Set { key, value } => {
                WriteRequest::Set { key, value, expires_at: None }
            },
            | ClientAction::SetWithExpiry { key, value, expiry } => {
                let expires_at = expiry.timestamp_millis() as u64;

                WriteRequest::Set { key, value, expires_at: Some(expires_at) }
            },
            | ClientAction::Append { key, value } => WriteRequest::Append { key, value },
            | ClientAction::Delete { keys } => WriteRequest::Delete { keys },
            | ClientAction::Incr { key } => WriteRequest::Incr { key, delta: 1 },
            | ClientAction::Decr { key } => WriteRequest::Decr { key, delta: 1 },
            | ClientAction::IncrBy { key, increment } => {
                WriteRequest::Incr { key, delta: increment }
            },
            | ClientAction::DecrBy { key, decrement } => {
                WriteRequest::Decr { key, delta: decrement }
            },
            | _ => {
                debug_assert!(false, "to_write_request called on non-write action: {self:?}");
                unreachable!(
                    "to_write_request should only be called after consensus_required() check"
                )
            },
        }
    }

    pub fn consensus_required(&self) -> bool {
        matches!(
            self,
            ClientAction::Set { .. }
                | ClientAction::SetWithExpiry { .. }
                | ClientAction::Append { .. }
                | ClientAction::Delete { .. }
                | ClientAction::Incr { .. }
                | ClientAction::Decr { .. }
                | ClientAction::IncrBy { .. }
                | ClientAction::DecrBy { .. }
        )
    }
}

#[derive(Clone, Debug)]
pub struct ClientRequest {
    pub(crate) action: ClientAction,
    pub(crate) session_req: SessionRequest,
}

impl ClientRequest {
    pub fn from_user_input(
        value: Vec<QueryIO>,
        session_req: SessionRequest,
    ) -> anyhow::Result<Self> {
        let mut values = value.into_iter().flat_map(|v| v.unpack_single_entry::<String>());
        let command = values.next().ok_or(anyhow::anyhow!("Unexpected command format"))?;
        let (command, args) = (command, values.collect::<Vec<_>>());

        Ok(ClientRequest {
            action: extract_action(&command, &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .map_err(|e| anyhow::anyhow!(e))?,
            session_req,
        })
    }
}

pub fn extract_action(action: &str, args: &[&str]) -> anyhow::Result<ClientAction> {
    // Check for invalid characters in command parts
    // Command-specific validation
    let cmd = action.to_uppercase();

    let require_exact_args = |count: usize| {
        if args.len() != count {
            Err(anyhow::anyhow!(
                "(error) ERR wrong number of arguments for '{}' command",
                cmd.to_lowercase()
            ))
        } else {
            Ok(())
        }
    };
    let require_non_empty_args = || {
        if args.is_empty() {
            Err(anyhow::anyhow!(
                "(error) ERR wrong number of arguments for '{}' command",
                cmd.to_lowercase()
            ))
        } else {
            Ok(())
        }
    };

    match cmd.as_str() {
        | "SET" => {
            if !(args.len() == 2 || (args.len() == 4 && args[2].eq_ignore_ascii_case("PX"))) {
                return Err(anyhow::anyhow!(
                    "(error) ERR wrong number of arguments for 'set' command"
                ));
            }
            if args.len() == 2 {
                return Ok(ClientAction::Set {
                    key: args[0].to_string(),
                    value: args[1].to_string(),
                });
            }
            Ok(ClientAction::SetWithExpiry {
                key: args[0].to_string(),
                value: args[1].to_string(),
                expiry: extract_expiry(args[3])?,
            })
        },

        | "APPEND" => {
            if args.len() != 2 {
                return Err(anyhow::anyhow!(
                    "(error) ERR wrong number of arguments for 'append' command"
                ));
            }
            Ok(ClientAction::Append { key: args[0].to_string(), value: args[1].to_string() })
        },

        | "GET" => {
            if args.len() == 1 {
                Ok(ClientAction::Get { key: args[0].to_string() })
            } else if args.len() == 2 {
                Ok(ClientAction::IndexGet { key: args[0].to_string(), index: args[1].parse()? })
            } else {
                return Err(anyhow::anyhow!(
                    "(error) ERR wrong number of arguments for 'get' command"
                ));
            }
        },

        | "KEYS" => {
            require_exact_args(1)?;

            if args[0] == "*" {
                Ok(ClientAction::Keys { pattern: None })
            } else {
                Ok(ClientAction::Keys { pattern: Some(args[0].to_string()) })
            }
        },
        | "DEL" => {
            require_non_empty_args()?;
            Ok(ClientAction::Delete { keys: args.iter().map(|s| s.to_string()).collect() })
        },
        | "EXISTS" => {
            require_non_empty_args()?;
            Ok(ClientAction::Exists { keys: args.iter().map(|s| s.to_string()).collect() })
        },

        | "PING" => {
            require_exact_args(0)?;
            Ok(ClientAction::Ping)
        },
        | "ECHO" => {
            require_exact_args(1)?;
            Ok(ClientAction::Echo(args[0].to_string()))
        },
        | "INFO" => {
            require_non_empty_args()?;
            Ok(ClientAction::Info)
        },

        | "CLUSTER" => {
            require_non_empty_args()?;
            match args[0].to_uppercase().as_str() {
                | "NODES" => Ok(ClientAction::ClusterNodes),
                | "INFO" => Ok(ClientAction::ClusterInfo),
                | "FORGET" => {
                    if args.len() != 2 {
                        return Err(anyhow::anyhow!(
                            "(error) ERR wrong number of arguments for 'cluster forget' command"
                        ));
                    }
                    Ok(ClientAction::ClusterForget(PeerIdentifier(args[1].bind_addr()?)))
                },
                | "MEET" => {
                    if args.len() == 2 {
                        return Ok(ClientAction::ClusterMeet(
                            PeerIdentifier(args[1].bind_addr()?),
                            LazyOption::Lazy,
                        ));
                    }
                    if args.len() == 3 {
                        // args[2].parse()? should be either lazy or eager
                        let lazy_option:LazyOption =FromStr::from_str(args[2]).context(
                            "(error) ERR wrong arguments for 'cluster meet' command, expected 'lazy' or 'eager'"
                        )?;

                        Ok(ClientAction::ClusterMeet(
                            PeerIdentifier(args[1].bind_addr()?),
                            lazy_option,
                        ))
                    } else {
                        Err(anyhow::anyhow!(
                            "(error) ERR wrong number of arguments for 'cluster meet' command"
                        ))
                    }
                },
                | "RESHARD" => Ok(ClientAction::ClusterReshard),
                | _ => Err(anyhow::anyhow!("(error) ERR unknown subcommand")),
            }
        },
        | "REPLICAOF" => {
            require_exact_args(2)?;
            Ok(ClientAction::ReplicaOf(PeerIdentifier::new(args[0], args[1].parse()?)))
        },
        | "ROLE" => {
            require_exact_args(0)?;
            Ok(ClientAction::Role)
        },
        | "CONFIG" => {
            require_exact_args(2)?;
            Ok(ClientAction::Config { key: args[0].to_string(), value: args[1].to_string() })
        },
        | "SAVE" => {
            require_exact_args(0)?;
            Ok(ClientAction::Save)
        },
        | "INCR" => {
            require_exact_args(1)?;
            Ok(ClientAction::Incr { key: args[0].to_string() })
        },
        | "DECR" => {
            require_exact_args(1)?;
            Ok(ClientAction::Decr { key: args[0].to_string() })
        },
        | "TTL" => {
            require_exact_args(1)?;
            Ok(ClientAction::Ttl { key: args[0].to_string() })
        },
        | "INCRBY" => {
            require_exact_args(2)?;

            let key = args[0].to_string();
            let increment = args[1].parse()?;
            Ok(ClientAction::IncrBy { key, increment })
        },
        | "DECRBY" => {
            require_exact_args(2)?;

            let key = args[0].to_string();
            let decrement = args[1].parse()?;
            Ok(ClientAction::DecrBy { key, decrement })
        },
        | "MGET" => {
            require_non_empty_args()?;
            Ok(ClientAction::MGet { keys: args.iter().map(|s| s.to_string()).collect() })
        },
        // Add other commands as needed
        | unknown_cmd => Err(anyhow::anyhow!(
            "(error) ERR unknown command '{unknown_cmd}', with args beginning with {}",
            args.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(" ")
        )),
    }
}

pub fn extract_expiry(expiry: &str) -> anyhow::Result<DateTime<Utc>> {
    let expiry = expiry.parse::<i64>().context("Invalid expiry")?;
    Ok(Utc::now() + chrono::Duration::milliseconds(expiry))
}
