use clap::Parser;
use secrecy::SecretString;
use std::path::PathBuf;

#[derive(Debug, Clone, Parser)]
pub struct BootstrapCommand {
    /// Comma-separated list of tool sources to install
    #[arg(long, value_delimiter = ',')]
    pub tools: Vec<String>,

    /// Comma-separated list of tool names to authenticate (default user)
    #[arg(long, value_delimiter = ',')]
    pub auth_tools: Vec<String>,
}

pub async fn run_bootstrap_command(cmd: BootstrapCommand) -> anyhow::Result<()> {
    let backend = std::env::var("DATABASE_BACKEND").unwrap_or_else(|_| "postgres".to_string());
    
    tracing::info!("Running database migrations for backend: {}", backend);
    match backend.as_str() {
        "libsql" | "turso" | "sqlite" => {
            #[cfg(feature = "libsql")]
            {
                use crate::db::Database as _;
                let db_path = std::env::var("LIBSQL_PATH")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| crate::config::default_libsql_path());
                
                let backend = if let Ok(url) = std::env::var("LIBSQL_URL") {
                    let token = std::env::var("LIBSQL_AUTH_TOKEN")
                        .map_err(|_| anyhow::anyhow!("LIBSQL_AUTH_TOKEN is required when LIBSQL_URL is set"))?;
                    crate::db::libsql::LibSqlBackend::new_remote_replica(&db_path, &url, &token).await?
                } else {
                    crate::db::libsql::LibSqlBackend::new_local(&db_path).await?
                };
                
                backend.run_migrations().await?;
                tracing::info!("libSQL migrations applied successfully");
            }
            #[cfg(not(feature = "libsql"))]
            {
                anyhow::bail!("libsql feature not compiled in");
            }
        }
        _ => {
            #[cfg(feature = "postgres")]
            {
                use crate::db::Database as _;
                let url = std::env::var("DATABASE_URL")
                    .map_err(|_| anyhow::anyhow!("DATABASE_URL not set"))?;
                
                let config = crate::config::DatabaseConfig {
                    backend: crate::config::DatabaseBackend::Postgres,
                    url: SecretString::new(url.into()),
                    ssl_mode: crate::config::SslMode::from_env(),
                    pool_size: 10,
                    libsql_path: None,
                    libsql_url: None,
                    libsql_auth_token: None,
                };
                let pg = crate::db::postgres::PgBackend::new(&config).await?;
                pg.run_migrations().await?;
                tracing::info!("PostgreSQL migrations applied successfully");
            }
            #[cfg(not(feature = "postgres"))]
            {
                anyhow::bail!("postgres feature not compiled in");
            }
        }
    }

    for tool_source in cmd.tools {
        if tool_source.trim().is_empty() { continue; }
        tracing::info!("Installing tool: {}", tool_source);
        let install_cmd = crate::cli::ToolCommand::Install {
            path: PathBuf::from(&tool_source),
            name: None,
            capabilities: None,
            target: None,
            release: true,
            skip_build: false,
            force: true, // Force re-install to ensure it's up to date
        };
        crate::cli::run_tool_command(install_cmd).await?;
    }

    for tool_name in cmd.auth_tools {
        if tool_name.trim().is_empty() { continue; }
        tracing::info!("Authenticating tool: {}", tool_name);
        let auth_cmd = crate::cli::ToolCommand::Auth {
            name: tool_name,
            dir: None,
            user: "default".to_string(),
        };
        crate::cli::run_tool_command(auth_cmd).await?;
    }

    Ok(())
}
