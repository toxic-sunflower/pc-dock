//! Передача файлов между клиентом и телефоном (по ADB), потоком через демон: на диск ПК ничего не пишется,
//! путь на ПК от клиента не принимается. Доступны только папки из списка разрешённых.

use std::io::Read;

use pcdock_adb::{check_remote_path, Adb, PushWriter};

/// Папки на телефоне, куда можно писать и откуда можно читать по умолчанию.
pub const DEFAULT_ROOTS: [&str; 3] = ["/sdcard", "/storage/emulated/0", "/data/local/tmp"];

/// Предельный размер одного файла.
pub const MAX_FILE: u64 = 8 * 1024 * 1024 * 1024;

/// Запись файла на телефон.
pub trait FileWriter: Send {
    fn write(&mut self, data: &[u8]) -> Result<(), String>;
    /// Завершает запись и проверяет итог: `size` — ожидаемый размер, `media` — сообщить галерее.
    fn finish(self: Box<Self>, size: u64, media: bool) -> Result<(), String>;
}

/// Что демону нужно от телефона для работы с файлами.
pub trait FileBackend: Send + Sync {
    fn put(&self, serial: &str, dest: &str) -> Result<Box<dyn FileWriter>, String>;
    /// Размер и читатель, который отдаёт ровно столько байт (иначе ошибка).
    fn get(&self, serial: &str, src: &str) -> Result<(u64, Box<dyn Read + Send>), String>;
    fn list(&self, serial: &str, path: &str) -> Result<Vec<String>, String>;
    fn delete(&self, serial: &str, path: &str) -> Result<(), String>;
}

/// Настройки передачи файлов.
pub struct FileOptions {
    pub backend: std::sync::Arc<dyn FileBackend>,
    /// Разрешённые папки на телефоне.
    pub roots: Vec<String>,
}

impl FileOptions {
    /// Файлы по ADB: `adb_server` — адрес сервера ADB (`None`: локальный).
    pub fn adb(adb_server: Option<String>, roots: Option<Vec<String>>) -> Self {
        let adb = match adb_server {
            Some(a) => Adb::new(a),
            None => Adb::local(),
        };
        Self {
            backend: std::sync::Arc::new(AdbFiles { adb }),
            roots: roots.unwrap_or_else(|| DEFAULT_ROOTS.iter().map(|s| s.to_string()).collect()),
        }
    }

    /// Проверяет путь: форма и разрешённая папка.
    pub fn check(&self, path: &str) -> Result<(), String> {
        check_remote_path(path)?;
        if self.roots.iter().any(|r| path == r || path.starts_with(&format!("{}/", r.trim_end_matches('/'))))
        {
            Ok(())
        } else {
            Err(format!("путь {path} вне разрешённых папок ({})", self.roots.join(", ")))
        }
    }
}

struct AdbFiles {
    adb: Adb,
}

struct AdbWriter {
    inner: PushWriter,
    adb: Adb,
    serial: String,
    dest: String,
}

impl FileWriter for AdbWriter {
    fn write(&mut self, data: &[u8]) -> Result<(), String> {
        self.inner.write(data)
    }

    fn finish(self: Box<Self>, size: u64, media: bool) -> Result<(), String> {
        let this = *self;
        this.inner.finish()?;
        // Команда не сообщает о нехватке места и правах: сверяем размер. Поток `exec:` закрывается сразу, а `cat` на
        // телефоне дописывает файл ещё какое-то время — ждём, пока размер дорастёт.
        wait_full_size(|| this.adb.file_size(&this.serial, &this.dest), size, SIZE_WAIT, std::thread::sleep)?;
        if media {
            let _ = this.adb.media_scan(&this.serial, &this.dest);
        }
        Ok(())
    }
}

/// Сколько ждать, пока записанный файл дорастёт до ожидаемого размера.
const SIZE_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// Ждёт, пока размер файла станет `size`: пока растёт или ещё мал — опрашивает (100 мс, удваивая до 1 с) до `limit`.
/// Больше ожидаемого или не дорос за `limit` — ошибка с последним размером.
fn wait_full_size(
    mut get: impl FnMut() -> Result<u64, String>,
    size: u64,
    limit: std::time::Duration,
    mut sleep: impl FnMut(std::time::Duration),
) -> Result<(), String> {
    let mut waited = std::time::Duration::ZERO;
    let mut step = std::time::Duration::from_millis(100);
    loop {
        let got = get()?;
        if got == size {
            return Ok(());
        }
        if got > size || waited >= limit {
            return Err(format!("записано {got} из {size} байт (нет места или прав?)"));
        }
        sleep(step);
        waited += step;
        step = (step * 2).min(std::time::Duration::from_secs(1));
    }
}

/// Читатель файла с проверкой, что отдан весь файл.
struct PullReader {
    inner: std::net::TcpStream,
    size: u64,
    read: u64,
}

impl Read for PullReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.read >= self.size {
            return Ok(0);
        }
        let want = buf.len().min((self.size - self.read) as usize);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "файл прочитан не полностью"));
        }
        self.read += n as u64;
        Ok(n)
    }
}

impl FileBackend for AdbFiles {
    fn put(&self, serial: &str, dest: &str) -> Result<Box<dyn FileWriter>, String> {
        let inner = self.adb.open_push(serial, dest)?;
        Ok(Box::new(AdbWriter {
            inner,
            adb: Adb::new(self.adb_addr()),
            serial: serial.to_string(),
            dest: dest.to_string(),
        }))
    }

    fn get(&self, serial: &str, src: &str) -> Result<(u64, Box<dyn Read + Send>), String> {
        let size = self.adb.file_size(serial, src)?;
        let inner = self.adb.open_pull(serial, src)?;
        Ok((size, Box::new(PullReader { inner, size, read: 0 })))
    }

    fn list(&self, serial: &str, path: &str) -> Result<Vec<String>, String> {
        self.adb.list_dir(serial, path)
    }

    fn delete(&self, serial: &str, path: &str) -> Result<(), String> {
        self.adb.remove_file(serial, path)
    }
}

impl AdbFiles {
    fn adb_addr(&self) -> String {
        self.adb.addr().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> FileOptions {
        FileOptions::adb(None, None)
    }

    /// `cat` на телефоне дописывает файл после закрытия потока: сначала 0, потом часть, потом весь
    #[test]
    fn waits_until_file_is_full() {
        let mut sizes = [0u64, 0, 40_000, 92_068].into_iter();
        let mut slept = Vec::new();
        let r = wait_full_size(|| Ok(sizes.next().unwrap()), 92_068, SIZE_WAIT, |d| slept.push(d));
        assert_eq!(r, Ok(()));
        assert_eq!(slept.len(), 3);
    }

    #[test]
    fn short_file_fails_after_limit() {
        let mut slept = std::time::Duration::ZERO;
        let r = wait_full_size(|| Ok(10), 100, std::time::Duration::from_secs(3), |d| slept += d);
        assert_eq!(r, Err("записано 10 из 100 байт (нет места или прав?)".to_string()));
        assert!(
            slept >= std::time::Duration::from_secs(3) && slept < std::time::Duration::from_secs(5),
            "{slept:?}"
        );
    }

    #[test]
    fn bigger_file_or_stat_error_fails_at_once() {
        let mut calls = 0;
        assert!(wait_full_size(
            || {
                calls += 1;
                Ok(200)
            },
            100,
            SIZE_WAIT,
            |_| {}
        )
        .is_err());
        assert_eq!(calls, 1);
        let r = wait_full_size(|| Err("нет файла /sdcard/x".to_string()), 100, SIZE_WAIT, |_| {});
        assert_eq!(r, Err("нет файла /sdcard/x".to_string()));
    }

    #[test]
    fn only_allowed_folders_pass() {
        let o = opts();
        for ok in ["/sdcard/a.png", "/sdcard", "/storage/emulated/0/DCIM/x.jpg", "/data/local/tmp/f"] {
            assert!(o.check(ok).is_ok(), "{ok}");
        }
        for bad in
            ["/data/data/com.app/db", "/system/bin/sh", "/sdcardx/a", "/sdcard/../data", "/", "/etc/hosts"]
        {
            assert!(o.check(bad).is_err(), "{bad}");
        }
    }
}
