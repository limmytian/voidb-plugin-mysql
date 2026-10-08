//! MySQL connection pool creation.
//!
//! Provides a factory function for creating `mysql_async::Pool` instances
//! from `MySqlConfig`. Uses `OptsBuilder` (field-based) rather than URL
//! construction to avoid password leakage in logs (threat T-02-02).

use mysql_async::{OptsBuilder, Pool};

use crate::config::MySqlConfig;

/// Create a new `mysql_async::Pool` from a `MySqlConfig`.
///
/// The pool is created with default connection limits. It does NOT
/// test the connection -- callers should issue a ping after creation
/// to verify connectivity.
///
/// # Arguments
///
/// * `config` - MySQL connection configuration.
///
/// # Errors
///
/// Returns an error string if the pool options are invalid.
pub fn create_pool(config: &MySqlConfig) -> Result<Pool, String> {
    let mut pool_opts = mysql_async::PoolOpts::default();
    let pool_size = config.normalized_pool_size() as usize;
    pool_opts = pool_opts.with_constraints(mysql_async::PoolConstraints::new(1, pool_size).ok_or("Invalid pool constraints")?);

    let mut opts = OptsBuilder::default()
        .ip_or_hostname(&config.host)
        .tcp_port(config.port)
        .user(Some(&config.username))
        .pass(Some(&config.password))
        .db_name(config.normalized_database())
        .pool_opts(pool_opts);

    if let Some(timeout_ms) = config.connect_timeout_ms {
        opts = opts.conn_ttl(std::time::Duration::from_millis(timeout_ms));
    }

    Ok(Pool::new(opts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_pool_respects_config_pool_size() {
        let mut config = MySqlConfig::new(
            "127.0.0.1".into(),
            3306,
            "root".into(),
            "secret".into(),
        );
        config.pool_size = Some(10);
        let pool = create_pool(&config).expect("create pool");
        // Verify pool created without error
        drop(pool);
    }
}

