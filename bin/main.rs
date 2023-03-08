use std::sync::Arc;

use async_nats::jetstream::{self, stream::Config, Context};
use clap::Parser;
use tokio::sync::Semaphore;
use tracing::{debug, error, trace};

use wadm::consumers::{
    manager::{ConsumerManager, WorkResult, Worker},
    *,
};
use wadm::{
    commands::*, events::*, DEFAULT_COMMANDS_TOPIC, DEFAULT_EVENTS_TOPIC, DEFAULT_EXPIRY_TIME,
};

mod logging;

#[derive(Parser, Debug)]
#[command(name = clap::crate_name!(), version = clap::crate_version!(), about = "wasmCloud Application Deployment Manager", long_about = None)]
struct Args {
    /// The ID for this wadm process. Defaults to a random UUIDv4 if none is provided. This is used
    /// to help with debugging when identifying which process is doing the work
    #[arg(short = 'i', long = "host-id", env = "WADM_HOST_ID")]
    host_id: Option<String>,

    /// Whether or not to use structured log output (as JSON)
    #[arg(
        short = 's',
        long = "structured-logging",
        default_value = "false",
        env = "WADM_STRUCTURED_LOGGING"
    )]
    structured_logging: bool,

    /// Whether or not to enable opentelemetry tracing
    #[arg(
        short = 't',
        long = "tracing",
        default_value = "false",
        env = "WADM_TRACING_ENABLED"
    )]
    tracing_enabled: bool,

    /// The endpoint to use for tracing. Setting this flag enables tracing, even if --tracing is set
    /// to false. Defaults to http://localhost:55681/v1/traces if not set and tracing is enabled
    #[arg(short = 'e', long = "tracing-endpoint", env = "WADM_TRACING_ENDPOINT")]
    tracing_endpoint: Option<String>,

    /// Name of the events stream to use
    #[arg(
        long = "event-stream",
        default_value = "wadm_events",
        env = "WADM_EVENT_STREAM"
    )]
    event_stream_name: String,

    /// Name of the commands stream to use
    #[arg(
        long = "command-stream",
        default_value = "wadm_commands",
        env = "WADM_COMMAND_STREAM"
    )]
    command_stream_name: String,

    /// The NATS JetStream domain to connect to
    #[arg(short = 'd', env = "WADM_JETSTREAM_DOMAIN")]
    domain: Option<String>,

    /// (Advanced) Tweak the maximum number of jobs to run for handling events and commands. Be
    /// careful how you use this as it can affect performance
    #[arg(
        short = 'j',
        long = "max-jobs",
        default_value = "256",
        env = "WADM_MAX_JOBS"
    )]
    max_jobs: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    logging::configure_tracing(
        args.structured_logging,
        args.tracing_enabled,
        args.tracing_endpoint,
    );

    // TODO: All the NATS connection options and jetstream stuff
    let client = async_nats::connect("127.0.0.1:4222").await?;
    let context = if let Some(domain) = args.domain {
        jetstream::with_domain(client.clone(), domain)
    } else {
        jetstream::new(client.clone())
    };

    let event_stream = context
        .get_or_create_stream(Config {
            name: args.event_stream_name,
            description: Some(
                "A stream that stores all events coming in on the wasmbus.evt topics in a cluster"
                    .to_string(),
            ),
            num_replicas: 1,
            retention: async_nats::jetstream::stream::RetentionPolicy::WorkQueue,
            subjects: vec![DEFAULT_EVENTS_TOPIC.to_owned()],
            max_age: DEFAULT_EXPIRY_TIME,
            storage: async_nats::jetstream::stream::StorageType::File,
            allow_rollup: false,
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;

    let command_stream = context
        .get_or_create_stream(Config {
            name: args.command_stream_name,
            description: Some("A stream that stores all commands for wadm".to_string()),
            num_replicas: 1,
            retention: async_nats::jetstream::stream::RetentionPolicy::WorkQueue,
            subjects: vec![DEFAULT_COMMANDS_TOPIC.to_owned()],
            max_age: DEFAULT_EXPIRY_TIME,
            storage: async_nats::jetstream::stream::StorageType::File,
            allow_rollup: false,
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;

    let host_id = args
        .host_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let permit_pool = Arc::new(Semaphore::new(args.max_jobs));
    let events_manager: ConsumerManager<EventConsumer> =
        ConsumerManager::new(permit_pool.clone(), event_stream);
    let commands_manager: ConsumerManager<CommandConsumer> =
        ConsumerManager::new(permit_pool.clone(), command_stream);
    events_manager
        .add_for_lattice("wasmbus.evt.default", EventWorker { context, host_id })
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    commands_manager
        .add_for_lattice("wadm.cmd.default", CommandWorker)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    tokio::signal::ctrl_c().await?;
    Ok(())
}

// Everything blow here will likely be moved when we do full implementation

struct EventWorker {
    context: Context,
    host_id: String,
}

#[async_trait::async_trait]
impl Worker for EventWorker {
    type Message = Event;

    async fn do_work(&self, mut message: ScopedMessage<Self::Message>) -> WorkResult<()> {
        debug!(event = ?message.as_ref(), "Handling received event");
        // THIS IS WHERE WE'D DO REAL WORK
        message.ack().await?;

        // NOTE: There is a possible race condition here where we send the command, and it
        // doesn't work even though we've acked the message. Worst case here is that we end
        // up not starting/creating something, which would be fixed on the next heartbeat.
        // This is better than the other option of double starting something when the ack
        // fails (think if something resulted in starting 100 actors on a host and then it
        // did it again)
        if let Err(e) = send_fake_command(&self.context, &self.host_id, &message).await {
            error!(error = %e, "Got error when sending command, will have to catch up on next reconcile");
        }

        Ok(())
    }
}

struct CommandWorker;

#[async_trait::async_trait]
impl Worker for CommandWorker {
    type Message = Command;

    async fn do_work(&self, mut message: ScopedMessage<Self::Message>) -> WorkResult<()> {
        debug!(event = ?message.as_ref(), "Handling received command");
        message.ack().await?;

        // NOTE: There is a possible race condition here where we send the lattice control
        // message, and it doesn't work even though we've acked the message. Worst case here
        // is that we end up not starting/creating something, which would be fixed on the
        // next heartbeat. This is better than the other option of double starting something
        // when the ack fails (think if something resulted in starting 100 actors on a host
        // and then it did it again)

        // THIS IS WHERE WE'D DO REAL WORK
        trace!("I'm sending something to the lattice control topics!");

        Ok(())
    }
}

// For testing only, to send a fake command in response to an event
async fn send_fake_command(context: &Context, host_id: &str, event: &Event) -> anyhow::Result<()> {
    use wadm::nats_utils::ensure_send;
    let command: Command = match event {
        Event::ActorStarted(actor) => {
            StartActor {
                reference: actor.image_ref.clone(),
                // So we know where it came from
                host_id: host_id.to_owned(),
                count: 2,
            }
            .into()
        }
        Event::ProviderStopped(prov) => {
            StopProvider {
                // So we know where it came from
                contract_id: prov.contract_id.clone(),
                host_id: host_id.to_owned(),
                provider_id: prov.public_key.clone(),
                link_name: Some(prov.link_name.clone()),
            }
            .into()
        }
        Event::LinkdefSet(ld) => {
            PutLinkdef {
                // So we know where it came from
                contract_id: ld.linkdef.contract_id.clone(),
                actor_id: ld.linkdef.actor_id.clone(),
                provider_id: ld.linkdef.provider_id.clone(),
                link_name: ld.linkdef.link_name.clone(),
                values: vec![("wadm_host".to_string(), host_id.to_owned())]
                    .into_iter()
                    .collect(),
            }
            .into()
        }
        _ => {
            StopActor {
                // So we know where it came from
                actor_id: host_id.to_owned(),
                host_id: "notreal".to_string(),
                count: 2,
            }
            .into()
        }
    };
    trace!(?command, "Sending command");
    ensure_send(context, "wadm.cmd.default".to_string(), &command).await
}
