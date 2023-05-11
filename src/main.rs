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
    parser
);

use anyhow::anyhow;
use dotenvy::dotenv;
use extensions::CustomGuildImpl;
use poise::serenity_prelude as serenity;
use std::{env, sync::Arc};

pub struct Data {
    /// The framework.shard_manager, used to get the latency of the current shard in the ping command
    shard_manager: Arc<serenity::Mutex<serenity::ShardManager>>,
}
type Context<'a> = poise::Context<'a, Data, anyhow::Error>;

async fn on_message(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    _framework: poise::FrameworkContext<'_, Data, anyhow::Error>,
    _data: &Data,
) -> Result<(), anyhow::Error> {
    if msg.author.bot {
        return Ok(());
    }

    if msg.guild(ctx).is_none() {
        if drql::scanner::scan(msg.content.as_str()).count() > 0 {
            msg.reply(
                ctx,
                "DRQL queries are only supported in guilds, not in DMs.",
            )
            .await?;
        }
        return Ok(());
    }

    let Some(ast) = 
        drql::scanner::scan(msg.content.as_str())
            .map(drql::parser::parse_drql)
            // TODO: Report errors as 'error in chunk X'?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .reduce(|acc, chunk| crate::drql::ast::Expr::Union(Box::new(acc), Box::new(chunk)))
    else {
        return Ok(());
    };

    let guild = msg.guild(ctx).ok_or(anyhow!("Unable to resolve guild"))?;

    let members_to_ping = match drql::interpreter::interpret(
        ast,
        &mut resolver::Resolver {
            guild: &guild,
            member: &msg.member(ctx).await?,
            ctx,
        },
    )
    .await
    {
        Ok(ast) => ast,
        Err(e) => {
            msg.reply(
                ctx,
                format!("An error occurred while calculating the result: {}", e),
            )
            .await?;
            return Ok(());
        }
    };

    // Now that we know which members we have to notify, we can do some specialized calculations
    // to try to replace members in that set with existing roles in the server. First, we choose our
    // "qualifiers" -- any role in this server that is a **subset** of our members_to_ping.

    // A hashmap of every role in the guild and its members.
    let roles_and_their_members = guild.all_roles_and_members(ctx)?;

    // next, we represent the list of users as a bunch of roles containing them and one outliers set.
    let util::unionize_set::UnionizeSetResult { sets, outliers } =
        util::unionize_set::unionize_set(&members_to_ping, &roles_and_their_members);

    // if members_to_ping.len() > 50 {
    //     // TODO: Ask the user to confirm they wish to do this action
    // }

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
        .into_keys()
        .copied()
        .map(models::mention::Mention::Role)
        .chain(outliers.into_iter().map(models::mention::Mention::User))
        .map(|x| x.to_string())
        .collect::<Vec<_>>();

    if stringified_mentions.is_empty() {
        msg.reply(ctx, "No users matched.").await?;
        return Ok(());
    }

    let notification_string = format!(
        concat!(
            "Notification triggered by Intersection.\n",
            ":question: **What is this?** Run {} for more information.\n"
        ),
        util::mention_application_command(ctx, "about").await?
    );

    if stringified_mentions.join(" ").len() <= (2000 - notification_string.len()) {
        msg.reply(
            ctx,
            format!("{}{}", notification_string, stringified_mentions.join(" ")),
        )
        .await?;
    } else {
        let messages = util::wrap_string_vec(stringified_mentions, " ", 2000)?;
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
                util::mention_application_command(ctx, "about").await?
            ),
        )
        .await?;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // We ignore the error because environment variables may be passed
    // in directly, and .env might not exist (e.g. in Docker with --env-file)
    let _ = dotenv();

    let framework: poise::FrameworkBuilder<Data, anyhow::Error> = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![commands::ping(), commands::about(), commands::debug()],
            event_handler: |ctx, event, framework, data| {
                Box::pin(async move {
                    if let poise::Event::Message { new_message } = event {
                        on_message(ctx, new_message, framework, data).await
                    } else {
                        Ok(())
                    }
                })
            },

            ..Default::default()
        })
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
