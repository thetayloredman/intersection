mod drql;
mod models;
mod schema;

#[macro_use]
extern crate lalrpop_util;

lalrpop_mod!(
    /// Direct access to the DRQL LALRPOP parser. Prefer to use the functions exported by drql::parser instead.
    #[allow(clippy::all)]
    parser
);

use std::{
    collections::{HashSet, VecDeque},
    env,
};

use anyhow::anyhow;
use async_recursion::async_recursion;
use diesel::{
    r2d2::{ConnectionManager, Pool},
    result::Error::NotFound,
    ExpressionMethods, QueryDsl, RunQueryDsl, SqliteConnection,
};
use dotenvy::dotenv;
use drql::ast;
use models::Guild;
use serenity::{
    async_trait,
    model::{
        prelude::{Activity, Message, Ready, RoleId},
        user::OnlineStatus,
    },
    prelude::*,
};

use crate::{drql::ast::Expr, models::NewGuild};

struct DB;

impl TypeMapKey for DB {
    type Value = Pool<ConnectionManager<SqliteConnection>>;
}

#[derive(Debug, PartialEq)]
pub enum ResolvedExpr {
    Union(Box<ResolvedExpr>, Box<ResolvedExpr>),
    Intersection(Box<ResolvedExpr>, Box<ResolvedExpr>),
    Difference(Box<ResolvedExpr>, Box<ResolvedExpr>),

    Everyone,
    Here,
    UserID(String),
    RoleID(String),
}

struct Handler;

struct CommandExecution<'a> {
    ctx: &'a Context,
    msg: &'a Message,
    guild: Guild,
    command: &'a str,
    args: VecDeque<&'a str>,
}

/// Function to fold an iterator of ASTs into one large union expression
fn reduce_ast_chunks(iter: impl Iterator<Item = ast::Expr>) -> Option<ast::Expr> {
    iter.reduce(|acc, chunk| ast::Expr::Union(Box::new(acc), Box::new(chunk)))
}

/// Function called whenever a **message-based command** is triggered.
async fn handle_command(data: CommandExecution<'_>) -> anyhow::Result<()> {
    let CommandExecution {
        ctx,
        msg,
        guild,
        command,
        mut args,
    } = data;

    if command == "config" {
        if !msg.member(ctx).await?.permissions(ctx)?.manage_guild() {
            msg.reply(
                ctx,
                "You need the Manage Server permission to run this command!",
            )
            .await?;
            return Ok(());
        }

        let subcommand = match args.pop_front() {
            Some(subcommand) => subcommand.to_lowercase(),
            None => {
                msg.reply(
                    ctx,
                    format!(
                        "You need to specify a subcommand. Try `{}config help`",
                        guild.prefix
                    ),
                )
                .await?;
                return Ok(());
            }
        };

        if subcommand == "help" {
            msg.reply(ctx, "Available subcommands: `prefix`, `help`")
                .await?;
        } else if subcommand == "prefix" {
            let action = match args.pop_front() {
                Some(action) => action.to_lowercase(),
                None => {
                    msg.reply(ctx, "Specify an action verb, `get` or `set`.")
                        .await?;
                    return Ok(());
                }
            };

            if action == "set" {
                if args.is_empty() {
                    msg.reply(
                        ctx,
                        format!(
                            "You need to specify a prefix. Try `{}config prefix set <prefix>`",
                            guild.prefix
                        ),
                    )
                    .await?;
                    return Ok(());
                }

                let new_prefix = args.make_contiguous().join(" ");

                // Obtain a connection to the database
                let mut conn = ctx
                    .data
                    .read()
                    .await
                    .get::<DB>()
                    .ok_or(anyhow!("DB was None"))?
                    .get()?;

                diesel::update(schema::guilds::table)
                    .filter(
                        schema::guilds::id.eq(msg
                            .guild_id
                            .ok_or(anyhow!("msg.guild_id was None"))?
                            .to_string()),
                    )
                    .set(schema::guilds::prefix.eq(new_prefix.as_str()))
                    .execute(&mut conn)?;

                msg.reply(
                    ctx,
                    format!("This server's prefix has been set to `{}`.", new_prefix),
                )
                .await?;
            } else if action == "get" {
                msg.reply(
                    ctx,
                    format!("This server's prefix is set to `{}`.", guild.prefix),
                )
                .await?;
            } else {
                msg.reply(
                    ctx,
                    format!(
                        "Unknown action verb. Try `{}config prefix get` or `{}config prefix set`.",
                        guild.prefix, guild.prefix
                    ),
                )
                .await?;
            }
        } else {
            msg.reply(
                ctx,
                format!("Unknown subcommand. Try `{}config help`", guild.prefix),
            )
            .await?;
        }
    } else if command == "run" {
        let Some(ast) = reduce_ast_chunks(
            drql::scanner::scan(args.make_contiguous().join(" ").as_str())
                .map(drql::parser::parse_drql)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter(),
        ) else {
            msg.reply(ctx, "Your message does not contain any DRQL queries to attempt to resolve").await?;
            return Ok(());
        };
        #[async_recursion]
        async fn walk_node(
            msg: &Message,
            ctx: &Context,
            node: Expr,
        ) -> anyhow::Result<ResolvedExpr> {
            Ok(match node {
                Expr::Difference(left, right) => ResolvedExpr::Difference(
                    Box::new(walk_node(msg, ctx, *left).await?),
                    Box::new(walk_node(msg, ctx, *right).await?),
                ),
                Expr::Intersection(left, right) => ResolvedExpr::Intersection(
                    Box::new(walk_node(msg, ctx, *left).await?),
                    Box::new(walk_node(msg, ctx, *right).await?),
                ),
                Expr::Union(left, right) => ResolvedExpr::Union(
                    Box::new(walk_node(msg, ctx, *left).await?),
                    Box::new(walk_node(msg, ctx, *right).await?),
                ),
                Expr::RoleID(id) => ResolvedExpr::RoleID(id),
                Expr::UserID(id) => ResolvedExpr::UserID(id),
                Expr::UnknownID(id) => {
                    if Some(id.clone()) == msg.guild_id.map(|id| id.to_string()) {
                        ResolvedExpr::Everyone
                    } else {
                        let guild = msg.guild(ctx).ok_or(anyhow!("Unable to resolve guild"))?;
                        let possible_member = guild.member(ctx, id.parse::<u64>()?).await;
                        if let Ok(member) = possible_member {
                            ResolvedExpr::UserID(member.user.id.to_string())
                        } else {
                            let possible_role = guild.roles.get(&RoleId::from(id.parse::<u64>()?));
                            if let Some(role) = possible_role {
                                if !role.mentionable
                                    && msg.member(ctx).await?.permissions(ctx)?.mention_everyone()
                                {
                                    anyhow::bail!("The role {} is not mentionable and you do not have the \"Mention everyone, here, and All Roles\" permission.", role.name);
                                }
                                ResolvedExpr::RoleID(role.id.to_string())
                            } else {
                                anyhow::bail!("Unable to resolve role or member ID: {}", id);
                            }
                        }
                    }
                }
                Expr::StringLiteral(s) => {
                    if s == "everyone" {
                        if !msg.member(ctx).await?.permissions(ctx)?.mention_everyone() {
                            anyhow::bail!("You do not have the \"Mention everyone, here, and All Roles\" permission required to use the role everyone.");
                        }
                        ResolvedExpr::Everyone
                    } else if s == "here" {
                        if !msg.member(ctx).await?.permissions(ctx)?.mention_everyone() {
                            anyhow::bail!("You do not have the \"Mention everyone, here, and All Roles\" permission required to use the role here.");
                        }
                        ResolvedExpr::Here
                    } else {
                        let guild = msg.guild(ctx).ok_or(anyhow!("Unable to resolve guild"))?;
                        if let Some((_, role)) = guild
                            .roles
                            .iter()
                            .find(|(_, value)| value.name.to_lowercase() == s.to_lowercase())
                        {
                            if !role.mentionable
                                && msg.member(ctx).await?.permissions(ctx)?.mention_everyone()
                            {
                                anyhow::bail!("The role {} is not mentionable and you do not have the \"Mention everyone, here, and All Roles\" permission.", role.name);
                            }
                            ResolvedExpr::RoleID(role.id.to_string())
                        } else if let Some((_, member)) = guild
                            .members // FIXME: what if the members aren't cached?
                            .iter()
                            .find(|(_, value)| value.user.tag().to_lowercase() == s.to_lowercase())
                        {
                            ResolvedExpr::UserID(member.user.id.to_string())
                        } else {
                            anyhow::bail!(
                            "Unable to resolve role or member **username** (use a tag like \"User#1234\" and no nickname!): {}",
                            s
                        );
                        }
                    }
                }
            })
        }
        let ast = match walk_node(msg, ctx, ast).await {
            Ok(ast) => ast,
            Err(e) => {
                msg.reply(ctx, format!("Error resolving: {}", e)).await?;
                return Ok(());
            }
        };

        // Walk over the AST one more time and resolve stuff to the final output
        #[async_recursion]
        async fn reduce_ast(
            msg: &Message,
            ctx: &Context,
            node: ResolvedExpr,
        ) -> anyhow::Result<HashSet<String>> {
            Ok(match node {
                ResolvedExpr::Difference(left, right) => reduce_ast(msg, ctx, *left)
                    .await?
                    .difference(&reduce_ast(msg, ctx, *right).await?)
                    .map(|x| x.to_string())
                    .collect::<HashSet<_>>(),
                ResolvedExpr::Union(left, right) => reduce_ast(msg, ctx, *left)
                    .await?
                    .union(&reduce_ast(msg, ctx, *right).await?)
                    .map(|x| x.to_string())
                    .collect::<HashSet<_>>(),
                ResolvedExpr::Intersection(left, right) => reduce_ast(msg, ctx, *left)
                    .await?
                    .intersection(&reduce_ast(msg, ctx, *right).await?)
                    .map(|x| x.to_string())
                    .collect::<HashSet<_>>(),
                ResolvedExpr::UserID(id) => {
                    let mut set = HashSet::new();
                    set.insert(id);
                    set
                }
                ResolvedExpr::RoleID(id) => {
                    let guild = msg.guild(ctx).ok_or(anyhow!("Unable to resolve guild"))?;
                    let role = guild
                        .roles
                        .get(&RoleId::from(id.parse::<u64>()?))
                        .ok_or(anyhow!("Unable to resolve role"))?;
                    let mut set = HashSet::new();
                    for member in guild.members.values() {
                        if member.roles.contains(&role.id) {
                            set.insert(member.user.id.to_string());
                        }
                    }
                    set
                }
                ResolvedExpr::Everyone => {
                    let guild = msg.guild(ctx).ok_or(anyhow!("Unable to resolve guild"))?;
                    let mut set = HashSet::new();
                    for member in guild.members.values() {
                        set.insert(member.user.id.to_string());
                    }
                    set
                }
                ResolvedExpr::Here => {
                    let guild = msg.guild(ctx).ok_or(anyhow!("Unable to resolve guild"))?;
                    let mut set = HashSet::new();
                    for member in guild.members.values() {
                        if let Some(presence) = guild.presences.get(&member.user.id) {
                            if presence.status != OnlineStatus::Offline {
                                set.insert(member.user.id.to_string());
                            }
                        }
                    }
                    set
                }
            })
        }
        let ast = match reduce_ast(msg, ctx, ast).await {
            Ok(ast) => ast,
            Err(e) => {
                msg.reply(ctx, format!("Error reducing: {}", e)).await?;
                return Ok(());
            }
        };
        msg.reply(
            ctx,
            format!(
                "{}",
                ast.iter()
                    .map(|id| format!("<@{}>", id))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        )
        .await?;
    } else {
        msg.reply(ctx, "Unknown command.").await?;
    }

    Ok(())
}

/// Obtain a [Guild] instance
async fn obtain_guild(ctx: &Context, guild_id: &str) -> anyhow::Result<Guild> {
    use schema::guilds::dsl::*;

    let mut conn = ctx
        .data
        .read()
        .await
        .get::<DB>()
        .ok_or(anyhow!("DB was None"))?
        .get()?;

    Ok(
        match guilds.filter(id.eq(guild_id)).first::<Guild>(&mut conn) {
            Ok(guild) => guild,
            Err(NotFound) => {
                let new_guild = NewGuild {
                    id: guild_id,
                    prefix: None,
                };

                diesel::insert_into(guilds)
                    .values(&new_guild)
                    .execute(&mut conn)?;

                // Re-do the query now that we have inserted
                guilds.filter(id.eq(guild_id)).first::<Guild>(&mut conn)?
            }
            Err(e) => return Err(e.into()),
        },
    )
}

/// Function called on every message.
async fn handle_message(ctx: &Context, msg: &Message) -> anyhow::Result<()> {
    if msg.author.bot {
        return Ok(());
    }

    if msg.channel(ctx).await?.guild().is_none() {
        msg.reply(ctx, "This bot only works in servers.").await?;
        return Ok(());
    }

    // Get this Guild from the database
    let guild = obtain_guild(
        ctx,
        msg.guild_id
            .ok_or(anyhow!("msg.guild_id was None"))?
            .to_string()
            .as_str(),
    )
    .await?;

    // TODO: Guide the user if they mention the bot instead of a prefix

    if !msg.content.starts_with(&guild.prefix) {
        return Ok(());
    }

    let mut args = msg.content[guild.prefix.len()..]
        .split_whitespace()
        .collect::<VecDeque<_>>();

    let command = match args.pop_front() {
        Some(command) => command,
        None => return Ok(()),
    };

    println!(
        "Command {} run by {} ({}) with args \"{}\"",
        command,
        msg.author.tag(),
        msg.author.id,
        args.make_contiguous().join(" ")
    );

    handle_command(CommandExecution {
        ctx,
        msg,
        guild,
        command,
        args,
    })
    .await
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        let result = handle_message(&ctx, &msg).await;

        if let Err(e) = result {
            if let Err(e2) = msg
                .reply(
                    ctx,
                    format!(
                        "An internal error occurred while processing your command: {}",
                        e
                    ),
                )
                .await
            {
                println!("An error occurred while handling an error. {:?}", e2);
            }
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("Logged in as {}!", ready.user.tag());
        ctx.set_activity(Activity::watching("for custom mentions"))
            .await;
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    dotenv()?;

    let database_url = env::var("DATABASE_URL").expect("Expected DATABASE_URL in the environment");
    let pool = Pool::builder()
        .test_on_check_out(true)
        .build(ConnectionManager::<SqliteConnection>::new(database_url))?;

    let intents = GatewayIntents::all();

    let mut client = Client::builder(
        env::var("TOKEN").expect("Expected a token in the environment"),
        intents,
    )
    .event_handler(Handler)
    .await?;

    {
        let mut data = client.data.write().await;
        data.insert::<DB>(pool);
    }

    client.start().await?;

    Ok(())
}
