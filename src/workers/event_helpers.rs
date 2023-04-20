use std::collections::HashMap;

use tracing::{instrument, warn};
use wasmcloud_control_interface::HostInventory;

use crate::{commands::Command, publisher::Publisher};

/// A subset of needed claims to help populate state
#[derive(Debug, Clone)]
pub struct Claims {
    pub name: String,
    pub capabilities: Vec<String>,
    pub issuer: String,
}

/// A trait for anything that can fetch a set of claims information about actors.
///
/// NOTE: This trait right now exists as a convenience for two things: First, testing. Without
/// something like this we require a network connection to unit test. Second, there is no concrete
/// claims type returned from the control interface client. This allows us to abstract that away
/// until such time that we do export one and we'll be able to do so without breaking our API
#[async_trait::async_trait]
pub trait ClaimsSource {
    async fn get_claims(&self) -> anyhow::Result<HashMap<String, Claims>>;
}

/// NOTE(brooksmtownsend): This trait exists in order to query the hosts inventory
/// upon receiving a heartbeat since the heartbeat doesn't contain enough
/// information to properly update the stored data for actors
#[async_trait::async_trait]
pub trait InventorySource {
    async fn get_inventory(&self, host_id: &str) -> anyhow::Result<HostInventory>;
}

#[async_trait::async_trait]
impl ClaimsSource for wasmcloud_control_interface::Client {
    async fn get_claims(&self) -> anyhow::Result<HashMap<String, Claims>> {
        Ok(self
            .get_claims()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .claims
            .into_iter()
            .filter_map(|mut claim| {
                // NOTE(thomastaylor312): I'm removing instead of getting since we own the data and I
                // don't want to clone every time we do this

                // If we don't find a subject, we can't actually get the actor ID, so skip this one
                Some((
                    claim.remove("sub")?,
                    Claims {
                        name: claim.remove("name").unwrap_or_default(),
                        capabilities: claim
                            .remove("caps")
                            .map(|raw| raw.split(',').map(|s| s.to_owned()).collect())
                            .unwrap_or_default(),
                        issuer: claim.remove("iss").unwrap_or_default(),
                    },
                ))
            })
            .collect())
    }
}

#[async_trait::async_trait]
impl InventorySource for wasmcloud_control_interface::Client {
    async fn get_inventory(&self, host_id: &str) -> anyhow::Result<HostInventory> {
        Ok(self
            .get_host_inventory(host_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?)
    }
}

/// A struct for publishing commands
#[derive(Clone)]
pub struct CommandPublisher<Pub> {
    publisher: Pub,
    topic: String,
}

impl<Pub> CommandPublisher<Pub> {
    /// Creates an new command publisher configured with the given publisher that will send to the
    /// specified topic
    pub fn new(publisher: Pub, topic: &str) -> CommandPublisher<Pub> {
        CommandPublisher {
            publisher,
            topic: topic.to_owned(),
        }
    }
}

impl<Pub: Publisher> CommandPublisher<Pub> {
    #[instrument(level = "trace", skip(self))]
    pub async fn publish_commands(&self, commands: Vec<Command>) -> anyhow::Result<()> {
        futures::future::join_all(
            commands
                .into_iter()
                // Generally commands are purely internal to wadm and so shouldn't have an error serializing. If it does, warn and continue onward
                .filter_map(|command| {
                    match serde_json::to_vec(&command) {
                        Ok(data) => Some(data),
                        Err(e) => {
                            warn!(error = %e, ?command, "Got malformed command when trying to serialize. Skipping this command");
                            None
                        }
                    }
                })
                .map(|data| self.publisher.publish(data, Some(&self.topic))),
        )
        .await
        .into_iter()
        .collect::<anyhow::Result<()>>()
    }
}