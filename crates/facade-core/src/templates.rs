//! Ready-made apps: databases as an official image, a volume for their data, a
//! generated password and a connection URL that linked apps reference.

use crate::api::{CreateDatabase, DatabaseKind};
use crate::env::reference;
use crate::spec::{AppSpec, HealthCheck, Limits, Source, SpecError, Volume};
use crate::stack::ServiceSpec;

/// A variable to set on the new app: `(key, value, sensitive)`.
pub type EnvVar = (String, String, bool);

pub struct Database {
    pub spec: AppSpec,
    pub env: Vec<EnvVar>,
    /// The database app's variable holding the connection URL.
    pub url_var: &'static str,
}

impl DatabaseKind {
    pub fn default_version(self) -> &'static str {
        match self {
            DatabaseKind::Postgres => "18",
            DatabaseKind::Mysql => "8.4",
            DatabaseKind::Mariadb => "11",
            DatabaseKind::Redis => "8",
        }
    }

    pub fn url_var(self) -> &'static str {
        match self {
            DatabaseKind::Redis => "REDIS_URL",
            _ => "DATABASE_URL",
        }
    }

    pub fn port(self) -> u16 {
        match self {
            DatabaseKind::Postgres => 5432,
            DatabaseKind::Mysql | DatabaseKind::Mariadb => 3306,
            DatabaseKind::Redis => 6379,
        }
    }
}

/// A database as a stack service: its settings and variables.
pub struct DatabaseService {
    pub service: ServiceSpec,
    pub env: Vec<EnvVar>,
    pub url_var: &'static str,
}

/// The app for `req` on `node`; `password` makes a fresh random password.
pub fn database(
    req: &CreateDatabase,
    node: &str,
    password: impl FnMut() -> String,
) -> Result<Database, SpecError> {
    let parts = parts(
        req.kind,
        req.version.as_deref(),
        &req.name,
        &req.name,
        password,
    )?;
    let spec = AppSpec {
        name: req.name.clone(),
        node: node.to_string(),
        source: Source::Image {
            image: parts.image.clone(),
        },
        http_port: None,
        domains: vec![],
        redirects: vec![],
        ports: vec![],
        host_network: false,
        limits: Limits {
            memory_mb: req.memory_mb,
            cpus: None,
        },
        enabled: true,
        command: parts.command,
        release_command: None,
        healthcheck: default_healthcheck(&parts.image),
        volumes: vec![parts.volume],
        links: vec![],
        stack: None,
    };
    spec.validate()?;
    Ok(Database {
        spec,
        env: parts.env,
        url_var: req.kind.url_var(),
    })
}

/// A database service `service` of stack `stack`: other services reach it by
/// its service name.
pub fn database_service(
    kind: DatabaseKind,
    version: Option<&str>,
    stack: &str,
    service: &str,
    password: impl FnMut() -> String,
) -> Result<DatabaseService, SpecError> {
    let app = format!("{stack}.{service}");
    let parts = parts(kind, version, &app, service, password)?;
    Ok(DatabaseService {
        service: ServiceSpec {
            healthcheck: default_healthcheck(&parts.image),
            image: Some(parts.image),
            command: (!parts.command.is_empty()).then_some(parts.command),
            volumes: Some(vec![parts.volume]),
            ..Default::default()
        },
        env: parts.env,
        url_var: kind.url_var(),
    })
}

/// A readiness check for the database and cache images everyone uses, so the
/// services depending on them start once they accept connections. Their images
/// have none of their own.
pub fn default_healthcheck(image: &str) -> Option<HealthCheck> {
    let name = image.rsplit('/').next().unwrap_or(image);
    let name = name.split([':', '@']).next().unwrap_or(name);
    // Initialisation runs a temporary server on the socket only: TCP means ready.
    let command = match name {
        "postgres" | "postgis" | "timescaledb" => {
            "pg_isready -h 127.0.0.1 -U \"${POSTGRES_USER:-postgres}\""
        }
        "mysql" => "mysqladmin ping -h 127.0.0.1 --silent",
        "mariadb" => "healthcheck.sh --connect --innodb_initialized",
        "redis" | "valkey" | "keydb" => "redis-cli ping || valkey-cli ping",
        "mongo" => "mongosh --quiet --eval 'quit(db.runCommand({ping: 1}).ok ? 0 : 1)'",
        _ => return None,
    };
    Some(HealthCheck::shell(command))
}

struct Parts {
    image: String,
    volume: Volume,
    env: Vec<EnvVar>,
    command: Vec<String>,
}

/// What every database needs: `app` is the app whose variables the URL refers to,
/// `name` the host it is reached at (and its database's name).
fn parts(
    kind: DatabaseKind,
    version: Option<&str>,
    app: &str,
    name: &str,
    mut password: impl FnMut() -> String,
) -> Result<Parts, SpecError> {
    let version = version
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(kind.default_version());
    if !version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c))
    {
        return Err(SpecError(format!("`{version}` is not an image tag")));
    }
    let db = name.replace('-', "_");
    let port = kind.port();
    let plain = |k: &str, v: String| (k.to_string(), v, false);
    let secret = |k: &str, v: String| (k.to_string(), v, true);
    let (image, data_path, env, command) = match kind {
        DatabaseKind::Postgres => {
            // From 18 on, the image keeps its data in a versioned directory under
            // /var/lib/postgresql and expects the volume there.
            let major = version
                .split(['.', '-'])
                .next()
                .and_then(|m| m.parse::<u32>().ok());
            let path = if major.is_some_and(|m| m < 18) {
                "/var/lib/postgresql/data"
            } else {
                "/var/lib/postgresql"
            };
            let url = format!(
                "postgres://postgres:{}@{name}:{port}/{db}",
                reference(app, "POSTGRES_PASSWORD")
            );
            (
                format!("postgres:{version}"),
                path,
                vec![
                    secret("POSTGRES_PASSWORD", password()),
                    plain("POSTGRES_DB", db.clone()),
                    plain("DATABASE_URL", url),
                ],
                vec![],
            )
        }
        DatabaseKind::Mysql | DatabaseKind::Mariadb => {
            let prefix = if kind == DatabaseKind::Mysql {
                "MYSQL"
            } else {
                "MARIADB"
            };
            let url = format!(
                "mysql://app:{}@{name}:{port}/{db}",
                reference(app, &format!("{prefix}_PASSWORD"))
            );
            let image = if kind == DatabaseKind::Mysql {
                "mysql"
            } else {
                "mariadb"
            };
            (
                format!("{image}:{version}"),
                "/var/lib/mysql",
                vec![
                    secret(&format!("{prefix}_ROOT_PASSWORD"), password()),
                    secret(&format!("{prefix}_PASSWORD"), password()),
                    plain(&format!("{prefix}_USER"), "app".into()),
                    plain(&format!("{prefix}_DATABASE"), db.clone()),
                    plain("DATABASE_URL", url),
                ],
                vec![],
            )
        }
        // Only reachable from linked apps, so no password; persisted to its volume.
        DatabaseKind::Redis => (
            format!("redis:{version}"),
            "/data",
            vec![plain("REDIS_URL", format!("redis://{name}:{port}"))],
            vec![
                "redis-server".to_string(),
                "--appendonly".into(),
                "yes".into(),
            ],
        ),
    };
    Ok(Parts {
        image,
        volume: Volume {
            name: "data".into(),
            path: data_path.into(),
        },
        env,
        command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: DatabaseKind, version: Option<&str>) -> CreateDatabase {
        CreateDatabase {
            name: "shop-db".into(),
            kind,
            version: version.map(str::to_string),
            node: None,
            memory_mb: None,
            link: vec![],
            env_var: None,
        }
    }

    #[test]
    fn postgres_paths_follow_the_version() {
        let pw = || "secret".to_string();
        let db = database(&request(DatabaseKind::Postgres, None), "main", pw).unwrap();
        assert_eq!(db.spec.volumes[0].to_string(), "data:/var/lib/postgresql");
        let url = &db.env.iter().find(|e| e.0 == "DATABASE_URL").unwrap().1;
        assert_eq!(
            url,
            "postgres://postgres:${{shop-db.POSTGRES_PASSWORD}}@shop-db:5432/shop_db"
        );
        let old = database(&request(DatabaseKind::Postgres, Some("16")), "main", pw).unwrap();
        assert_eq!(old.spec.volumes[0].path, "/var/lib/postgresql/data");
        assert!(database(&request(DatabaseKind::Redis, Some("8 ; rm")), "main", pw).is_err());
        let svc = database_service(DatabaseKind::Postgres, None, "shop", "db", pw).unwrap();
        let url = &svc.env.iter().find(|e| e.0 == "DATABASE_URL").unwrap().1;
        assert_eq!(
            url,
            "postgres://postgres:${{shop.db.POSTGRES_PASSWORD}}@db:5432/db"
        );
    }
}
