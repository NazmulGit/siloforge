//! Where v1 rows come from: the live container (`docker exec ... psql COPY ... TO STDOUT`, read-only,
//! no host port needed) or a directory of `<table>.csv` files written by [`super::export_v1_to_dir`].

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};

use sf_core::{Error, Result};

/// NULL marker used in our COPY commands (`NULL '\N'`), so empty strings and NULLs stay distinct.
pub const NULL_MARK: &str = "\\N";

/// A source of v1 table data as CSV with a header row.
pub trait Source: Send + Sync {
    /// Column names of `table`, or None when the table does not exist in the source.
    fn columns(&self, table: &str) -> Result<Option<Vec<String>>>;
    /// Streams `SELECT cols FROM table [WHERE filter]` as CSV with header. Sources that cannot filter
    /// (CSV directories) ignore `filter`; the importer re-applies it.
    fn open(
        &self,
        table: &str,
        cols: &[String],
        filter: Option<&str>,
    ) -> Result<Box<dyn Read + Send>>;
    /// Fast row count when the source supports it (None = caller must stream).
    fn count(&self, table: &str, filter: Option<&str>) -> Result<Option<u64>>;
    /// Short description for reports.
    fn describe(&self) -> String;
}

/// Live v1 Postgres reached through `docker exec <container> psql`.
#[derive(Debug, Clone)]
pub struct DockerSource {
    pub docker: String,
    pub container: String,
    pub user: String,
    pub database: String,
}

impl Default for DockerSource {
    fn default() -> Self {
        Self {
            docker: "docker".into(),
            container: "siloforge_pg".into(),
            user: "siloforge".into(),
            database: "siloforge".into(),
        }
    }
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

impl DockerSource {
    fn psql(&self) -> Result<Command> {
        // I-04: these are separate argv entries (no shell), but a value starting with `-` would
        // be read by docker/psql as an option.
        for (what, v) in [
            ("container", &self.container),
            ("user", &self.user),
            ("database", &self.database),
        ] {
            if v.is_empty() || v.starts_with('-') {
                return Err(Error::bad(format!("invalid v1 {what} name '{v}'")));
            }
        }
        let mut c = Command::new(&self.docker);
        c.args([
            "exec",
            &self.container,
            "psql",
            "-X",
            "-q",
            "-v",
            "ON_ERROR_STOP=1",
            "-U",
            &self.user,
            "-d",
            &self.database,
            // every session is read-only: the migrator can never write to v1
            "-c",
            "SET default_transaction_read_only = on",
        ]);
        c.stdin(Stdio::null());
        Ok(c)
    }

    /// Runs a scalar/list query (`-At`) and returns stdout lines.
    pub fn query_lines(&self, sql: &str) -> Result<Vec<String>> {
        let out = self
            .psql()?
            .args(["-A", "-t", "-c", sql])
            .output()
            .map_err(|e| Error::upstream(format!("docker exec failed: {e}")))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(Error::upstream(format!(
                "psql failed: {}",
                err.trim().chars().take(500).collect::<String>()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && l != "SET")
            .collect())
    }

    /// Checks that the container answers.
    pub fn check(&self) -> Result<()> {
        let l = self.query_lines("SELECT 1")?;
        if l.first().map(String::as_str) == Some("1") {
            Ok(())
        } else {
            Err(Error::upstream("unexpected psql output"))
        }
    }
}

impl Source for DockerSource {
    fn columns(&self, table: &str) -> Result<Option<Vec<String>>> {
        let t = table.replace('\'', "''");
        let cols = self.query_lines(&format!(
            "SELECT column_name FROM information_schema.columns WHERE table_schema='public' AND table_name='{t}' ORDER BY ordinal_position"
        ))?;
        Ok((!cols.is_empty()).then_some(cols))
    }

    fn open(
        &self,
        table: &str,
        cols: &[String],
        filter: Option<&str>,
    ) -> Result<Box<dyn Read + Send>> {
        let list = cols
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let wh = filter.map(|f| format!(" WHERE {f}")).unwrap_or_default();
        let sql = format!(
            "COPY (SELECT {list} FROM {}{wh}) TO STDOUT WITH (FORMAT csv, HEADER true, NULL '{NULL_MARK}')",
            quote_ident(table)
        );
        let mut child = self
            .psql()?
            .args(["-c", &sql])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::upstream(format!("docker exec failed: {e}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::internal("no stdout"))?;
        let stderr = child.stderr.take();
        Ok(Box::new(ChildReader {
            child,
            stdout,
            stderr,
            done: false,
        }))
    }

    fn count(&self, table: &str, filter: Option<&str>) -> Result<Option<u64>> {
        let wh = filter.map(|f| format!(" WHERE {f}")).unwrap_or_default();
        let l = self.query_lines(&format!("SELECT count(*) FROM {}{wh}", quote_ident(table)))?;
        Ok(l.first().and_then(|s| s.parse().ok()))
    }

    fn describe(&self) -> String {
        format!(
            "docker exec {} psql -U {} -d {}",
            self.container, self.user, self.database
        )
    }
}

/// Streams a child's stdout; on EOF waits for it and turns a non-zero exit into an error with stderr.
struct ChildReader {
    child: Child,
    stdout: ChildStdout,
    stderr: Option<ChildStderr>,
    done: bool,
}

impl Read for ChildReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.stdout.read(buf)?;
        if n == 0 && !self.done {
            self.done = true;
            let status = self.child.wait()?;
            if !status.success() {
                let mut err = String::new();
                if let Some(mut e) = self.stderr.take() {
                    let _ = e.by_ref().take(64 * 1024).read_to_string(&mut err);
                }
                return Err(std::io::Error::other(format!(
                    "psql exited with {status}: {}",
                    err.trim().chars().take(500).collect::<String>()
                )));
            }
        }
        Ok(n)
    }
}

impl Drop for ChildReader {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A directory of `<table>.csv` files (header row, `\N` = NULL).
#[derive(Debug, Clone)]
pub struct DirSource {
    pub dir: PathBuf,
}

impl DirSource {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }
    fn path(&self, table: &str) -> PathBuf {
        self.dir.join(format!("{table}.csv"))
    }
}

impl Source for DirSource {
    fn columns(&self, table: &str) -> Result<Option<Vec<String>>> {
        let p = self.path(table);
        if !p.is_file() {
            return Ok(None);
        }
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_reader(std::fs::File::open(&p)?);
        let h = rdr
            .byte_headers()
            .map_err(|e| Error::bad(format!("{}: {e}", p.display())))?;
        Ok(Some(
            h.iter()
                .map(|c| String::from_utf8_lossy(c).trim().to_string())
                .collect(),
        ))
    }

    fn open(
        &self,
        table: &str,
        _cols: &[String],
        _filter: Option<&str>,
    ) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(std::io::BufReader::with_capacity(
            1 << 20,
            std::fs::File::open(self.path(table))?,
        )))
    }

    fn count(&self, _table: &str, _filter: Option<&str>) -> Result<Option<u64>> {
        Ok(None)
    }

    fn describe(&self) -> String {
        format!("csv dir {}", self.dir.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_like_names_refused() {
        // I-04: never hand docker/psql an argument that looks like an option.
        let src = DockerSource {
            container: "--privileged".into(),
            ..Default::default()
        };
        assert!(
            src.check()
                .is_err_and(|e| e.to_string().contains("container"))
        );
        let src = DockerSource {
            database: "-h".into(),
            ..Default::default()
        };
        assert!(
            src.query_lines("SELECT 1")
                .is_err_and(|e| e.to_string().contains("database"))
        );
    }
}
