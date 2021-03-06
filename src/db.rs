//! Database operations

use postgres::{Connection, SslMode};
use postgres::error::ConnectError;


const DB_CONNECTION_STR: &'static str = "postgresql://cratesfyi@localhost";


/// Connects to database
pub fn connect_db() -> Result<Connection, ConnectError> {
    Connection::connect(DB_CONNECTION_STR, SslMode::None)
}


/// Creates database tables
pub fn create_tables(conn: &Connection) {
    let queries = [
        "CREATE TABLE crates ( \
            id SERIAL, \
            name text UNIQUE NOT NULL, \
            latest_version_id INT DEFAULT 0, \
            stars INT DEFAULT 0, \
            issues JSON, \
            versions JSON DEFAULT '[]', \
            downloads_total INT DEFAULT 0, \
            github_last_update TIMESTAMP \
        )",
        "CREATE TABLE releases ( \
            id SERIAL, \
            crate_id INT NOT NULL, \
            version TEXT, \
            release_time TIMESTAMP, \
            dependencies JSON, \
            yanked BOOL DEFAULT FALSE, \
            build_status INT DEFAULT 0, \
            rustdoc_status INT DEFAULT 0, \
            test_status INT DEFAULT 0, \
            license TEXT, \
            repository_url TEXT, \
            homepage_url TEXT, \
            description TEXT, \
            description_long TEXT, \
            readme TEXT, \
            authors JSON, \
            keywords JSON, \
            have_examples BOOL DEFAULT FALSE, \
            downloads INT DEFAULT 0, \
            UNIQUE (crate_id, version) \
        )",
        "CREATE TABLE authors ( \
            id SERIAL, \
            name TEXT NOT NULL, \
            email TEXT, \
            slug TEXT UNIQUE NOT NULL \
        )",
        "CREATE TABLE author_rels ( \
            rid INT, \
            aid INT, \
            UNIQUE(rid, aid) \
        )",
        "CREATE TABLE keywords ( \
            id SERIAL, \
            name TEXT, \
            slug TEXT NOT NULL UNIQUE \
        )",
        "CREATE TABLE keyword_rels ( \
            rid INT, \
            kid INT, \
            UNIQUE(rid, kid) \
        )",
        "CREATE TABLE owners ( \
            id SERIAL, \
            login TEXT NOT NULL UNIQUE, \
            slug TEXT NOT NULL UNIQUE, \
            avatar TEXT, \
            name TEXT, \
            email TEXT \
        )",
        "CREATE TABLE owner_rels ( \
            cid INT, \
            oid INT, \
            UNIQUE(cid, oid) \
        )"
    ];

    for query in queries.into_iter() {
        if let Err(e) = conn.execute(query, &[]) {
            println!("{}", e);
        }
    }
}



#[test]
#[ignore]
fn test_connect_db() {
    let conn = connect_db();
    assert!(conn.is_ok());
}
