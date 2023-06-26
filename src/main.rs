#![doc = include_str!("../README.md")]
#![warn(clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![warn(clippy::nursery)]
#![warn(clippy::pedantic)]
#![allow(clippy::unused_async)] // command functions must be async
#![warn(missing_docs)]

mod commands;
mod drql;
mod extensions;
mod models;
mod resolver;
mod util;

#[macro_use]
extern crate lalrpop_util;

lalrpop_mod!(
    /// Direct access to the DRQL LALRPOP parser. Prefer to use the functions exported by drql::parser instead.
    #[allow(clippy::all)]
    #[allow(clippy::nursery)]
    #[allow(clippy::pedantic)]
    parser
);

use anyhow::{bail, Context as _};
use dotenvy::dotenv;
use extensions::CustomGuildImpl;
use poise::serenity_prelude as serenity;
use std::{env, sync::Arc};

/// Information collected when compiled, by crate `built`
pub mod build_info {
    // File is inserted by build.rs
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

pub struct Data {
    /// The framework.shard_manager, used to get the latency of the current shard in the ping command
    shard_manager: Arc<serenity::Mutex<serenity::ShardManager>>,
}
type Context<'a> = poise::Context<'a, Data, anyhow::Error>;

async fn handle_drql_query(ctx: &serenity::Context, msg: &serenity::Message) -> anyhow::Result<()> {
    if msg.guild(ctx).is_none() {
        bail!("DRQL queries are not available in DMs.");
    }

    let ast = drql::scanner::scan(msg.content.as_str())
        .enumerate()
        .map(|(n, chunk)| {
            drql::parser::parse_drql(chunk).context(format!("Error parsing chunk {n}"))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .reduce(|acc, chunk| crate::drql::ast::Expr::Union(Box::new(acc), Box::new(chunk)))
        .context("There is no DRQL query in your message to handle.")?; // This should never happen, as we already checked that there was at least one chunk in the input

    let guild = msg.guild(ctx).context("Unable to resolve guild")?;

    let members_to_ping = drql::interpreter::interpret(
        ast,
        &mut resolver::Resolver {
            guild: &guild,
            member: &msg.member(ctx).await?,
            ctx,
        },
    )
    .await
    .context("Error calculating result")?;

    // Now that we know which members we have to notify, we can do some specialized calculations
    // to try to replace members in that set with existing roles in the server. First, we choose our
    // "qualifiers" -- any role in this server that is a **subset** of our members_to_ping.

    // A hashmap of every role in the guild and its members.
    let roles_and_their_members = guild.all_roles_and_members(ctx)?;

    // next, we represent the list of users as a bunch of roles containing them and one outliers set.
    let util::unionize_set::UnionizeSetResult { sets, outliers } =
        util::unionize_set::unionize_set(&members_to_ping, &roles_and_their_members);

    // Now we need to split the output message into individual pings. First, stringify each user mention...
    // TODO: Once message splitting is complete this could result in a user being
    // pinged multiple times if they are present in a role that is split into multiple
    // messages.
    // e.g.
    // user is in @A and @C
    // message 1: @A @B ...
    // message 2: @C @D ...
    // double ping!
    let stringified_mentions = sets
        .into_iter()
        .copied()
        .map(models::mention::Mention::Role)
        .chain(
            outliers
                .into_iter()
                .map(|&id| models::mention::Mention::User(id)),
        )
        .map(|x| x.to_string())
        .collect::<Vec<_>>();

    if members_to_ping.len() > 50 {
        let serenity::Channel::Guild(channel) = msg.channel(ctx).await? else {
            // DMs would have been prevented already.
            // Categories can't have messages sent duh
            bail!("unreachable");
        };
        let mut m = channel
            .send_message(ctx, |m| {
                m.content(format!(
                    concat!(
                        "**Hold up!** By running this query, you are about to",
                        " mention {} people.{} Are you sure?"
                    ),
                    members_to_ping.len(),
                    {
                        let len = util::wrap_string_vec(&stringified_mentions, " ", 2000)
                            .unwrap() // TODO: Remove unwrap?
                            .len();
                        if len > 2 {
                            format!(" This will require the sending of {len} messages.")
                        } else {
                            String::new()
                        }
                    }
                ))
                .reference_message(msg) // basically makes it a reply
                .components(|components| {
                    components.create_action_row(|action_row| {
                        action_row
                            .create_button(|button| {
                                button
                                    .custom_id("large_ping_confirm_no")
                                    .emoji(serenity::ReactionType::Unicode("❌".to_string()))
                                    .label("Cancel")
                                    .style(serenity::ButtonStyle::Secondary)
                            })
                            .create_button(|button| {
                                button
                                    .custom_id("large_ping_confirm_yes")
                                    .emoji(serenity::ReactionType::Unicode("✅".to_string()))
                                    .label("Yes")
                                    .style(serenity::ButtonStyle::Primary)
                            })
                    })
                })
            })
            .await?;

        let Some(interaction) = m.await_component_interaction(ctx)
            .collect_limit(1)
            .author_id(msg.author.id)
            .timeout(std::time::Duration::from_secs(30))
            .await else {
                m.edit(ctx, |m| m.content("Timed out waiting for confirmation.").components(|components| components)).await?;
                return Ok(());
            };

        if interaction.data.custom_id == "large_ping_confirm_no" {
            m.edit(ctx, |m| {
                m.content("Cancelled.").components(|components| components)
            })
            .await?;

            return Ok(());
        } else if interaction.data.custom_id == "large_ping_confirm_yes" {
            m.edit(ctx, |m| {
                m.content("Confirmed.").components(|components| components)
            })
            .await?;

            // let it continue!
        } else {
            unreachable!();
        }
    }

    if stringified_mentions.is_empty() {
        msg.reply(ctx, "No users matched.").await?;
        return Ok(());
    }

    let notification_string = format!(
        concat!(
            "Notification triggered by Intersection.\n",
            ":question: **What is this?** Run {} for more information.\n"
        ),
        util::mention_application_command(ctx, "about landing").await?
    );

    if stringified_mentions.join(" ").len() <= (2000 - notification_string.len()) {
        msg.reply(
            ctx,
            format!("{}{}", notification_string, stringified_mentions.join(" ")),
        )
        .await?;
    } else {
        let messages = util::wrap_string_vec(&stringified_mentions, " ", 2000)?;
        msg.reply(
            ctx,
            format!(
                "Notification triggered by Intersection. Please wait, sending {} messages...",
                messages.len()
            ),
        )
        .await?;
        for message in messages {
            msg.reply(ctx, message).await?;
        }
        msg.reply(
            ctx,
            format!(
                concat!(
                    "Notification triggered successfully.\n",
                    ":question: **What is this?** Run {} for more information."
                ),
                util::mention_application_command(ctx, "about landing").await?
            ),
        )
        .await?;
    }

    Ok(())
}

struct Handler;
#[serenity::async_trait]
impl serenity::EventHandler for Handler {
    async fn message(&self, ctx: serenity::Context, msg: serenity::Message) {
        if msg.author.bot {
            return;
        }

        if drql::scanner::scan(msg.content.as_str()).count() > 0 {
            match handle_drql_query(&ctx, &msg)
                .await
                .context("Error handling DRQL query")
            {
                Ok(_) => {}
                Err(query_err) => {
                    if let Err(message_send_err) = msg.reply(ctx, format!("{query_err:#}")).await {
                        panic!("Error sending error message: {message_send_err:#}");
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // We ignore the error because environment variables may be passed
    // in directly, and .env might not exist (e.g. in Docker with --env-file)
    let _ = dotenv();

    let framework: poise::FrameworkBuilder<Data, anyhow::Error> = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                commands::ping(),
                commands::about(),
                commands::debug(),
                commands::version(),
            ],

            ..Default::default()
        })
        .client_settings(|client| client.event_handler(Handler))
        .token(env::var("TOKEN").expect("Expected a token in the environment"))
        .intents(serenity::GatewayIntents::all())
        .setup(|ctx, ready, framework| {
            Box::pin(async move {
                println!(
                    "Logged in as {}#{}!",
                    ready.user.name, ready.user.discriminator
                );

                println!("Registering global application (/) commands...");
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                println!("Finished registering global application (/) commands.");

                Ok(Data {
                    shard_manager: Arc::clone(framework.shard_manager()),
                })
            })
        });

    Ok(framework.run().await?)
}
