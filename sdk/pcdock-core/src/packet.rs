//! Формат пакетов потока и сообщения протокола v2 (см. `devices/android/PROTOCOL.md`, §3-§7).
//!
//! Все числа big-endian, строки UTF-8.

/// Признак начала пакета: "PD".
pub const MAGIC: [u8; 2] = *b"PD";
/// Размер заголовка пакета.
pub const HEADER_LEN: usize = 8;
/// Максимальный размер полезной нагрузки пакета.
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// Версия протокола, которую реализует это ядро.
pub const PROTOCOL_VERSION: u16 = 2;

/// Возможности (битовая маска в `Hello::capabilities`).
pub mod caps {
    pub const VIDEO_JPEG: u32 = 1 << 0;
    pub const VIDEO_H264: u32 = 1 << 1;
    pub const AUDIO_PCM: u32 = 1 << 2;
    pub const AUDIO_OPUS: u32 = 1 << 3;
    pub const ROTATION: u32 = 1 << 4;
    pub const INPUT_OVER_STREAM: u32 = 1 << 5;
    /// Телефон сообщает пакет приложения на переднем плане (`STATUS` 16/17).
    pub const FOREGROUND: u32 = 1 << 6;
    /// Телефон открывает приложение по команде хоста (`LAUNCH_APP`, ответ — `STATUS` 18).
    pub const LAUNCH_APP: u32 = 1 << 7;
    /// Телефон отдаёт список приложений с главным экраном (`LIST_APPS`, ответ — `STATUS` 19).
    pub const LIST_APPS: u32 = 1 << 8;
    /// Телефон отдаёт элементы экрана (`UI_DUMP`, ответ — `STATUS` 20 частями) и сообщает «экран изменился» (`STATUS` 21).
    pub const UI_TREE: u32 = 1 << 9;
    /// Телефон отдаёт причины завершения процессов приложения (`EXIT_INFO`, ответ — `STATUS` 22; Android 11+).
    pub const EXIT_INFO: u32 = 1 << 10;
    /// Телефон делает HTTPS GET по своей сети по умолчанию и отвечает телом, кодом и типом сети (`IP_CHECK`, ответ —
    /// `STATUS` 23): внешний IP телефона.
    pub const IP_CHECK: u32 = 1 << 11;
    /// Телефон выполняет HTTP-запрос к устройству своей локальной Wi-Fi-сети (`HTTP_LOCAL`, ответ — `STATUS` 24):
    /// например, к админке роутера, в Wi-Fi которого телефон, а компьютер — нет.
    pub const HTTP_LOCAL: u32 = 1 << 12;
    /// Телефон вводит текст любого языка (кириллица, эмодзи) в поле, где стоит курсор (`TEXT_INPUT`, ответ — `STATUS`
    /// 25): вставкой через службу специальных возможностей или через буфер обмена, после которого хост жмёт Ctrl+V.
    pub const TEXT_INPUT: u32 = 1 << 13;
}

/// Коды кодеков.
pub mod codec {
    pub const VIDEO_JPEG: u8 = 1;
    pub const VIDEO_H264: u8 = 2;
    pub const AUDIO_PCM_S16LE: u8 = 1;
    pub const AUDIO_OPUS: u8 = 2;
}

/// Коды ошибок (`ErrorMsg::code`).
pub mod error_code {
    pub const UNSUPPORTED_VERSION: u16 = 1;
    pub const BAD_PACKET: u16 = 2;
    pub const INTERNAL: u16 = 3;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PacketType {
    Hello,
    Ping,
    Pong,
    DisplayInfo,
    VideoFrame,
    VideoConfig,
    AudioConfig,
    AudioFrame,
    RequestFrame,
    SetStream,
    /// Разбудить экран (хост → телефон, пустой): включает экран на минуту.
    Wake,
    /// Открыть приложение по имени пакета (хост → телефон, возможность `caps::LAUNCH_APP`).
    LaunchApp,
    /// Список приложений с главным экраном (хост → телефон, возможность `caps::LIST_APPS`).
    ListApps,
    /// Элементы экрана (хост → телефон, возможность `caps::UI_TREE`).
    UiDump,
    /// Причины завершения процессов приложения (хост → телефон, возможность `caps::EXIT_INFO`).
    ExitInfo,
    /// HTTPS-запрос по сети телефона — внешний IP (хост → телефон, возможность `caps::IP_CHECK`).
    IpCheck,
    /// HTTP-запрос к устройству локальной Wi-Fi-сети телефона (хост → телефон, возможность `caps::HTTP_LOCAL`).
    HttpLocal,
    /// Ввести текст в поле с курсором (хост → телефон, возможность `caps::TEXT_INPUT`).
    TextInput,
    Status,
    Error,
    /// Тип, неизвестный этой версии ядра (такие пакеты пропускаются).
    Unknown(u8),
}

impl PacketType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0x01 => Self::Hello,
            0x02 => Self::Ping,
            0x03 => Self::Pong,
            0x10 => Self::DisplayInfo,
            0x11 => Self::VideoFrame,
            0x12 => Self::VideoConfig,
            0x20 => Self::AudioConfig,
            0x21 => Self::AudioFrame,
            0x40 => Self::RequestFrame,
            0x41 => Self::SetStream,
            0x42 => Self::Wake,
            0x43 => Self::LaunchApp,
            0x44 => Self::ListApps,
            0x45 => Self::UiDump,
            0x46 => Self::ExitInfo,
            0x47 => Self::IpCheck,
            0x48 => Self::HttpLocal,
            0x49 => Self::TextInput,
            0x50 => Self::Status,
            0x7F => Self::Error,
            other => Self::Unknown(other),
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Self::Hello => 0x01,
            Self::Ping => 0x02,
            Self::Pong => 0x03,
            Self::DisplayInfo => 0x10,
            Self::VideoFrame => 0x11,
            Self::VideoConfig => 0x12,
            Self::AudioConfig => 0x20,
            Self::AudioFrame => 0x21,
            Self::RequestFrame => 0x40,
            Self::SetStream => 0x41,
            Self::Wake => 0x42,
            Self::LaunchApp => 0x43,
            Self::ListApps => 0x44,
            Self::UiDump => 0x45,
            Self::ExitInfo => 0x46,
            Self::IpCheck => 0x47,
            Self::HttpLocal => 0x48,
            Self::TextInput => 0x49,
            Self::Status => 0x50,
            Self::Error => 0x7F,
            Self::Unknown(v) => v,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub ptype: PacketType,
    pub flags: u8,
    pub payload: Vec<u8>,
}

impl Packet {
    pub fn new(ptype: PacketType, payload: Vec<u8>) -> Self {
        Self { ptype, flags: 0, payload }
    }

    pub fn with_flags(mut self, flags: u8) -> Self {
        self.flags = flags;
        self
    }

    /// Записывает пакет (заголовок и данные) в `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        debug_assert!(self.payload.len() <= MAX_PAYLOAD);
        out.extend_from_slice(&MAGIC);
        out.push(self.ptype.to_u8());
        out.push(self.flags);
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.payload);
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        self.encode_into(&mut out);
        out
    }
}

/// Потоковый разборщик пакетов. Принимает байты по мере поступления и отдаёт готовые пакеты.
///
/// При повреждении потока перестраивается (PROTOCOL.md §3): ищет следующее "PD" и принимает
/// кандидата, только если за ним идёт ещё один заголовок.
#[derive(Debug, Default)]
pub struct PacketReader {
    buf: Vec<u8>,
    resync: bool,
    dropped: u64,
}

impl PacketReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Сколько байт отброшено при перестройке потока.
    pub fn dropped_bytes(&self) -> u64 {
        self.dropped
    }

    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.resync = false;
    }

    pub fn next_packet(&mut self) -> Option<Packet> {
        loop {
            if self.buf.len() < HEADER_LEN {
                return None;
            }
            if self.buf[0..2] != MAGIC {
                self.skip_to_candidate();
                continue;
            }
            let len = u32::from_be_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize;
            if len > MAX_PAYLOAD {
                self.resync = true;
                self.drop_bytes(1);
                continue;
            }
            let total = HEADER_LEN + len;
            if self.buf.len() < total {
                return None;
            }
            if self.resync {
                // Кандидат после потери синхронизации: подтверждаем следующим заголовком,
                // если он уже пришёл.
                if self.buf.len() >= total + 2 && self.buf[total..total + 2] != MAGIC {
                    self.drop_bytes(1);
                    continue;
                }
                self.resync = false;
            }
            let packet = Packet {
                ptype: PacketType::from_u8(self.buf[2]),
                flags: self.buf[3],
                payload: self.buf[HEADER_LEN..total].to_vec(),
            };
            self.buf.drain(..total);
            return Some(packet);
        }
    }

    fn skip_to_candidate(&mut self) {
        self.resync = true;
        let pos = self.buf[1..].windows(2).position(|w| w == MAGIC).map(|p| p + 1);
        match pos {
            Some(p) => self.drop_bytes(p),
            None => {
                // Последний байт может оказаться началом "PD", оставляем его.
                let keep = usize::from(self.buf.last() == Some(&MAGIC[0]));
                let n = self.buf.len() - keep;
                self.drop_bytes(n);
            }
        }
    }

    fn drop_bytes(&mut self, n: usize) {
        self.buf.drain(..n);
        self.dropped += n as u64;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    Invalid(&'static str),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "пакет обрезан"),
            Self::Invalid(why) => write!(f, "некорректный пакет: {why}"),
        }
    }
}

impl std::error::Error for DecodeError {}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.data.len() - self.pos < n {
            return Err(DecodeError::Truncated);
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    fn rest(&mut self) -> &'a [u8] {
        let s = &self.data[self.pos..];
        self.pos = self.data.len();
        s
    }
}

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Host = 0,
    Device = 1,
}

/// Открытие сессии (PROTOCOL.md §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub version_max: u16,
    pub version_min: u16,
    pub capabilities: u32,
    pub role: Role,
    pub name: String,
}

impl Hello {
    pub fn to_packet(&self) -> Packet {
        let name = &self.name.as_bytes()[..self.name.len().min(255)];
        let mut p = Vec::with_capacity(10 + name.len());
        p.extend_from_slice(&self.version_max.to_be_bytes());
        p.extend_from_slice(&self.version_min.to_be_bytes());
        p.extend_from_slice(&self.capabilities.to_be_bytes());
        p.push(self.role as u8);
        p.push(name.len() as u8);
        p.extend_from_slice(name);
        Packet::new(PacketType::Hello, p)
    }

    pub fn from_payload(data: &[u8]) -> Result<Self, DecodeError> {
        let mut c = Cursor::new(data);
        let version_max = c.u16()?;
        let version_min = c.u16()?;
        let capabilities = c.u32()?;
        let role = match c.u8()? {
            0 => Role::Host,
            1 => Role::Device,
            _ => return Err(DecodeError::Invalid("неизвестная роль")),
        };
        let n = c.u8()? as usize;
        let name = utf8(c.take(n)?);
        Ok(Self { version_max, version_min, capabilities, role, name })
    }
}

/// Согласование версии и возможностей: `(версия сессии, общие возможности)`.
pub fn negotiate(local: &Hello, remote: &Hello) -> Result<(u16, u32), u16> {
    let version = local.version_max.min(remote.version_max);
    if version < local.version_min.max(remote.version_min) {
        return Err(error_code::UNSUPPORTED_VERSION);
    }
    Ok((version, local.capabilities & remote.capabilities))
}

/// Размеры экрана и поворот (PROTOCOL.md §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayInfo {
    /// Размер кадров, которые пойдут дальше.
    pub stream_width: u16,
    pub stream_height: u16,
    /// Реальный размер экрана телефона в пикселях в текущей ориентации.
    pub screen_width: u16,
    pub screen_height: u16,
    pub density_dpi: u16,
    /// 0..3 = поворот на 0°, 90°, 180°, 270°.
    pub rotation: u8,
}

impl DisplayInfo {
    pub fn to_packet(&self) -> Packet {
        let mut p = Vec::with_capacity(12);
        for v in
            [self.stream_width, self.stream_height, self.screen_width, self.screen_height, self.density_dpi]
        {
            p.extend_from_slice(&v.to_be_bytes());
        }
        p.push(self.rotation);
        p.push(0);
        Packet::new(PacketType::DisplayInfo, p)
    }

    pub fn from_payload(data: &[u8]) -> Result<Self, DecodeError> {
        let mut c = Cursor::new(data);
        let info = Self {
            stream_width: c.u16()?,
            stream_height: c.u16()?,
            screen_width: c.u16()?,
            screen_height: c.u16()?,
            density_dpi: c.u16()?,
            rotation: c.u8()?,
        };
        if info.rotation > 3 {
            return Err(DecodeError::Invalid("rotation должен быть 0..3"));
        }
        Ok(info)
    }
}

/// Кадр экрана.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
    pub codec: u8,
    pub key_frame: bool,
    pub pts_us: u64,
    pub data: Vec<u8>,
}

impl VideoFrame {
    pub fn to_packet(&self) -> Packet {
        let mut p = Vec::with_capacity(9 + self.data.len());
        p.push(self.codec);
        p.extend_from_slice(&self.pts_us.to_be_bytes());
        p.extend_from_slice(&self.data);
        Packet::new(PacketType::VideoFrame, p).with_flags(u8::from(self.key_frame))
    }

    pub fn from_packet(packet: &Packet) -> Result<Self, DecodeError> {
        let mut c = Cursor::new(&packet.payload);
        let codec = c.u8()?;
        let pts_us = c.u64()?;
        Ok(Self { codec, key_frame: packet.flags & 1 != 0, pts_us, data: c.rest().to_vec() })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioConfig {
    pub codec: u8,
    pub sample_rate: u32,
    pub channels: u8,
}

impl AudioConfig {
    pub fn to_packet(&self) -> Packet {
        let mut p = vec![self.codec];
        p.extend_from_slice(&self.sample_rate.to_be_bytes());
        p.push(self.channels);
        Packet::new(PacketType::AudioConfig, p)
    }

    pub fn from_payload(data: &[u8]) -> Result<Self, DecodeError> {
        let mut c = Cursor::new(data);
        Ok(Self { codec: c.u8()?, sample_rate: c.u32()?, channels: c.u8()? })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioFrame {
    pub pts_us: u64,
    pub data: Vec<u8>,
}

impl AudioFrame {
    pub fn to_packet(&self) -> Packet {
        let mut p = Vec::with_capacity(8 + self.data.len());
        p.extend_from_slice(&self.pts_us.to_be_bytes());
        p.extend_from_slice(&self.data);
        Packet::new(PacketType::AudioFrame, p)
    }

    pub fn from_payload(data: &[u8]) -> Result<Self, DecodeError> {
        let mut c = Cursor::new(data);
        let pts_us = c.u64()?;
        Ok(Self { pts_us, data: c.rest().to_vec() })
    }
}

/// Настройки потока, которые хост просит у телефона.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetStream {
    pub video: bool,
    pub audio: bool,
    /// 0 = по умолчанию.
    pub max_width: u16,
    pub max_height: u16,
    /// 0 = по умолчанию, иначе 1..100.
    pub jpeg_quality: u8,
    /// 0 = без ограничения.
    pub max_fps: u8,
}

impl Default for SetStream {
    fn default() -> Self {
        Self { video: true, audio: false, max_width: 0, max_height: 0, jpeg_quality: 0, max_fps: 0 }
    }
}

impl SetStream {
    pub fn to_packet(&self) -> Packet {
        let mut p = vec![u8::from(self.video) | (u8::from(self.audio) << 1)];
        p.extend_from_slice(&self.max_width.to_be_bytes());
        p.extend_from_slice(&self.max_height.to_be_bytes());
        p.push(self.jpeg_quality);
        p.push(self.max_fps);
        Packet::new(PacketType::SetStream, p)
    }

    pub fn from_payload(data: &[u8]) -> Result<Self, DecodeError> {
        let mut c = Cursor::new(data);
        let flags = c.u8()?;
        Ok(Self {
            video: flags & 1 != 0,
            audio: flags & 2 != 0,
            max_width: c.u16()?,
            max_height: c.u16()?,
            jpeg_quality: c.u8()?,
            max_fps: c.u8()?,
        })
    }
}

/// Событие на стороне телефона (`code` задаёт реализация приёмника, например "проекция остановлена").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub code: u16,
    pub detail: String,
}

impl Status {
    pub fn to_packet(&self) -> Packet {
        let mut p = self.code.to_be_bytes().to_vec();
        p.extend_from_slice(self.detail.as_bytes());
        Packet::new(PacketType::Status, p)
    }

    pub fn from_payload(data: &[u8]) -> Result<Self, DecodeError> {
        let mut c = Cursor::new(data);
        Ok(Self { code: c.u16()?, detail: utf8(c.rest()) })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorMsg {
    pub code: u16,
    pub message: String,
}

impl ErrorMsg {
    pub fn to_packet(&self) -> Packet {
        let mut p = self.code.to_be_bytes().to_vec();
        p.extend_from_slice(self.message.as_bytes());
        Packet::new(PacketType::Error, p)
    }

    pub fn from_payload(data: &[u8]) -> Result<Self, DecodeError> {
        let mut c = Cursor::new(data);
        Ok(Self { code: c.u16()?, message: utf8(c.rest()) })
    }
}

pub fn ping_packet(nonce: u64) -> Packet {
    Packet::new(PacketType::Ping, nonce.to_be_bytes().to_vec())
}

pub fn pong_packet(nonce: u64) -> Packet {
    Packet::new(PacketType::Pong, nonce.to_be_bytes().to_vec())
}

pub fn nonce_from_payload(data: &[u8]) -> Result<u64, DecodeError> {
    Cursor::new(data).u64()
}

pub fn request_frame_packet() -> Packet {
    Packet::new(PacketType::RequestFrame, Vec::new())
}

/// Разбудить экран телефона (пустой пакет, хост → телефон).
pub fn wake_packet() -> Packet {
    Packet::new(PacketType::Wake, Vec::new())
}

/// Запрос списка приложений (`LIST_APPS`, 0x44): `u32 request_id`. Телефон отвечает `STATUS` 19 с `detail` =
/// `"<request_id> <JSON-массив [{"package","label","icon"?}]>"`.
pub fn list_apps_packet(request_id: u32) -> Packet {
    Packet::new(PacketType::ListApps, request_id.to_be_bytes().to_vec())
}

/// Запрос элементов экрана (`UI_DUMP`, 0x45): `u32 request_id`. Телефон отвечает `STATUS` 20 одной или несколькими
/// частями `"<request_id> <i>/<n> <часть>"`; склеенные части — `"xml <дамп как у uiautomator>"`,
/// `"no_service"`, `"no_window"` или `"error <причина>"`.
pub fn ui_dump_packet(request_id: u32) -> Packet {
    Packet::new(PacketType::UiDump, request_id.to_be_bytes().to_vec())
}

/// Запрос причин завершения процессов приложения (`EXIT_INFO`, 0x46): `u32 request_id`, `u64 since_ms` (мс с 1970,
/// раньше — не присылать), затем имя пакета UTF-8 до конца. Ответ — `STATUS` 22 `"<request_id> <JSON | unsupported |
/// no_permission | invalid_package>"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitInfoReq {
    pub request_id: u32,
    pub since_ms: u64,
    pub package: String,
}

impl ExitInfoReq {
    pub fn to_packet(&self) -> Packet {
        let mut p = self.request_id.to_be_bytes().to_vec();
        p.extend_from_slice(&self.since_ms.to_be_bytes());
        p.extend_from_slice(self.package.as_bytes());
        Packet::new(PacketType::ExitInfo, p)
    }

    pub fn from_payload(data: &[u8]) -> Result<Self, DecodeError> {
        let mut c = Cursor::new(data);
        Ok(Self { request_id: c.u32()?, since_ms: c.u64()?, package: utf8(c.rest()) })
    }
}

/// Запрос «IP-адрес телефона» (`IP_CHECK`, 0x47): `u32 request_id`, затем URL UTF-8 до конца. Ответ — `STATUS` 23
/// `"<request_id> <JSON-объект>"`: `{"ok":true,"code":200,"body":"…","transport":"cellular","ms":312,"gateway":"…"}` или
/// `{"ok":false,"error":"timeout",…}` (PROTOCOL.md «IP-адрес телефона (возможность 11)»).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpCheckReq {
    pub request_id: u32,
    pub url: String,
}

impl IpCheckReq {
    pub fn to_packet(&self) -> Packet {
        let mut p = self.request_id.to_be_bytes().to_vec();
        p.extend_from_slice(self.url.as_bytes());
        Packet::new(PacketType::IpCheck, p)
    }

    pub fn from_payload(data: &[u8]) -> Result<Self, DecodeError> {
        let mut c = Cursor::new(data);
        Ok(Self { request_id: c.u32()?, url: utf8(c.rest()) })
    }
}

/// URL, который можно передать в `IP_CHECK` (те же правила проверяет телефон): только `https://`, не длиннее 512 байт,
/// без пробелов и управляющих символов, непустой адрес без `user@`.
pub fn valid_ip_check_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    if url.len() > 512 || url.chars().any(|c| (c as u32) <= 0x20 || c as u32 == 0x7f) {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    !authority.is_empty() && !authority.contains('@') && !authority.starts_with(':')
}

/// Наибольшее тело запроса `HTTP_LOCAL` (байт UTF-8); столько же телефон отдаёт из тела ответа.
pub const HTTP_LOCAL_MAX_BODY: usize = 64 * 1024;
/// Срок запроса `HTTP_LOCAL` по умолчанию (если `timeout_ms` = 0), мс.
pub const HTTP_LOCAL_DEFAULT_TIMEOUT_MS: u32 = 10_000;
/// Наибольший срок запроса `HTTP_LOCAL`, мс (больше — урезается).
pub const HTTP_LOCAL_MAX_TIMEOUT_MS: u32 = 30_000;
/// Наибольшее число заголовков запроса `HTTP_LOCAL`.
pub const HTTP_LOCAL_MAX_HEADERS: usize = 32;

/// HTTP-запрос к устройству локальной Wi-Fi-сети телефона (`HTTP_LOCAL`, 0x48): `u32 request_id`, затем JSON UTF-8
/// до конца `{"method":"GET"|"POST","url":"http://192.168.1.1/…","headers":{…},"body":"…","timeout_ms":10000}`.
/// Телефон делает запрос через свою Wi-Fi-сеть (даже если сеть по умолчанию — мобильная), без редиректов и cookie.
/// Ответ — `STATUS` 24 `"<request_id> <JSON-объект>"`: `{"ok":true,"code":401,"body":"…","headers":{…},"ms":35}` (любой
/// код HTTP — успех) или `{"ok":false,"error":"no_wifi","ms":1}` (PROTOCOL.md «HTTP в локальной сети (возможность 12)»).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpLocalReq {
    pub request_id: u32,
    /// `GET` или `POST`.
    pub method: String,
    /// `http://` или `https://` с адресом IPv4 частной или локальной сети (см. [`valid_http_local_url`]).
    pub url: String,
    /// Заголовки запроса по порядку; имена без повторов (без учёта регистра).
    pub headers: Vec<(String, String)>,
    /// Тело (только у `POST`), не больше [`HTTP_LOCAL_MAX_BODY`] байт.
    pub body: String,
    /// Срок, мс: 0 — [`HTTP_LOCAL_DEFAULT_TIMEOUT_MS`], больше [`HTTP_LOCAL_MAX_TIMEOUT_MS`] — урезается.
    pub timeout_ms: u32,
}

/// Почему запрос `HTTP_LOCAL` нельзя отправить.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpLocalInvalid {
    /// URL не подходит (см. [`valid_http_local_url`]).
    Url,
    /// Метод, заголовки или тело не подходят.
    Request,
}

impl HttpLocalReq {
    /// Срок, который уйдёт телефону.
    pub fn effective_timeout_ms(&self) -> u32 {
        match self.timeout_ms {
            0 => HTTP_LOCAL_DEFAULT_TIMEOUT_MS,
            t => t.min(HTTP_LOCAL_MAX_TIMEOUT_MS),
        }
    }

    /// Те же правила проверяет телефон: метод `GET`/`POST`, URL по [`valid_http_local_url`], не больше
    /// [`HTTP_LOCAL_MAX_HEADERS`] заголовков с именами-токенами HTTP (≤ 256 байт, без повторов) и значениями без
    /// управляющих символов (≤ 4096 байт), тело только у `POST` и не больше [`HTTP_LOCAL_MAX_BODY`].
    pub fn check(&self) -> Result<(), HttpLocalInvalid> {
        if !valid_http_local_url(&self.url) {
            return Err(HttpLocalInvalid::Url);
        }
        let post = match self.method.as_str() {
            "GET" => false,
            "POST" => true,
            _ => return Err(HttpLocalInvalid::Request),
        };
        if (!post && !self.body.is_empty()) || self.body.len() > HTTP_LOCAL_MAX_BODY {
            return Err(HttpLocalInvalid::Request);
        }
        if self.headers.len() > HTTP_LOCAL_MAX_HEADERS {
            return Err(HttpLocalInvalid::Request);
        }
        let tchar = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
        for (i, (name, value)) in self.headers.iter().enumerate() {
            if name.is_empty() || name.len() > 256 || !name.chars().all(tchar) {
                return Err(HttpLocalInvalid::Request);
            }
            if value.len() > 4096 || value.chars().any(|c| (c < ' ' && c != '\t') || c == '\u{7f}') {
                return Err(HttpLocalInvalid::Request);
            }
            if self.headers[..i].iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
                return Err(HttpLocalInvalid::Request);
            }
        }
        Ok(())
    }

    /// JSON запроса для телефона (одна строка): `headers` и `body` — только если не пустые.
    pub fn to_json(&self) -> String {
        let mut j = format!("{{\"method\":{},\"url\":{}", json_str(&self.method), json_str(&self.url));
        if !self.headers.is_empty() {
            j.push_str(",\"headers\":{");
            for (i, (n, v)) in self.headers.iter().enumerate() {
                if i > 0 {
                    j.push(',');
                }
                j.push_str(&json_str(n));
                j.push(':');
                j.push_str(&json_str(v));
            }
            j.push('}');
        }
        if !self.body.is_empty() {
            j.push_str(",\"body\":");
            j.push_str(&json_str(&self.body));
        }
        j.push_str(&format!(",\"timeout_ms\":{}}}", self.effective_timeout_ms()));
        j
    }

    pub fn to_packet(&self) -> Packet {
        let mut p = self.request_id.to_be_bytes().to_vec();
        p.extend_from_slice(self.to_json().as_bytes());
        Packet::new(PacketType::HttpLocal, p)
    }

    /// `request_id` и JSON запроса из payload (JSON ядро не разбирает — это делает телефон).
    pub fn split_payload(data: &[u8]) -> Result<(u32, String), DecodeError> {
        let mut c = Cursor::new(data);
        Ok((c.u32()?, utf8(c.rest())))
    }
}

/// Строка JSON в кавычках (кавычки, `\\` и управляющие символы экранируются).
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// URL, который можно передать в `HTTP_LOCAL` (те же правила проверяет телефон): `http://` или `https://`, не длиннее
/// 2048 байт, без пробелов и управляющих символов, без `user@`; адрес — только IPv4 цифрами (без имён и DNS) из
/// частных и локальных сетей: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16; порт (если есть) 1–65535.
pub fn valid_http_local_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) else {
        return false;
    };
    if url.len() > 2048 || url.chars().any(|c| (c as u32) <= 0x20 || c as u32 == 0x7f) {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if let Some(p) = port {
        let ok = !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit());
        if !ok || !(1..=65535).contains(&p.parse::<u32>().unwrap_or(0)) {
            return false;
        }
    }
    // Строго четыре десятичных числа 0–255 без ведущих нулей (как и на телефоне)
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    let mut o = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        let digits = !part.is_empty() && part.len() <= 3 && part.bytes().all(|b| b.is_ascii_digit());
        if !digits || (part.len() > 1 && part.starts_with('0')) {
            return false;
        }
        match part.parse::<u8>() {
            Ok(v) => o[i] = v,
            Err(_) => return false,
        }
    }
    o[0] == 10
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 169 && o[1] == 254)
}

/// Наибольший текст `TEXT_INPUT`, байт UTF-8 (подпись Instagram — до 2200 символов, с эмодзи — до ~9 КБ).
pub const TEXT_INPUT_MAX: usize = 16 * 1024;

/// Ввести текст в поле, где стоит курсор (`TEXT_INPUT`, 0x49): `u32 request_id`, затем текст UTF-8 до конца payload.
/// Телефон вставляет его в поле с фокусом ввода (служба специальных возможностей: «Вставить» из буфера обмена или
/// «Задать текст») или только кладёт в буфер обмена — тогда хост жмёт Ctrl+V своей HID-клавиатурой.
/// Ответ — `STATUS` 25 `"<request_id> <JSON-объект>"`: `{"ok":true,"method":"paste"|"set_text"|"clipboard","ms":12}`
/// или `{"ok":false,"error":"bad_request"|"clipboard: …"|"not_negotiated","ms":0}` (PROTOCOL.md «Ввод текста
/// (возможность 13)»).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextInputReq {
    pub request_id: u32,
    /// Не пустой, не больше [`TEXT_INPUT_MAX`] байт UTF-8, без символа `\0`.
    pub text: String,
}

impl TextInputReq {
    /// Те же правила проверяет телефон: текст не пустой, не больше [`TEXT_INPUT_MAX`] байт UTF-8 и без `\0`.
    pub fn valid(&self) -> bool {
        valid_text_input(&self.text)
    }

    pub fn to_packet(&self) -> Packet {
        let mut p = self.request_id.to_be_bytes().to_vec();
        p.extend_from_slice(self.text.as_bytes());
        Packet::new(PacketType::TextInput, p)
    }

    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let mut c = Cursor::new(data);
        Ok(Self { request_id: c.u32()?, text: utf8(c.rest()) })
    }
}

/// Текст, который можно передать в `TEXT_INPUT`: не пустой, не больше [`TEXT_INPUT_MAX`] байт UTF-8, без `\0`.
pub fn valid_text_input(text: &str) -> bool {
    !text.is_empty() && text.len() <= TEXT_INPUT_MAX && !text.contains('\0')
}

/// Имя пакета Android, которое можно передать в `LAUNCH_APP`: латинская буква, затем буквы, цифры, `_` и `.`,
/// не длиннее 255 байт (те же правила проверяет телефон).
pub fn valid_package(p: &str) -> bool {
    let mut ch = p.chars();
    matches!(ch.next(), Some(c) if c.is_ascii_alphabetic())
        && p.len() <= 255
        && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Команда «открыть приложение» (`LAUNCH_APP`, 0x43): `u32 request_id`, затем имя пакета UTF-8 до конца.
/// Телефон отвечает `STATUS` 18 с `detail` = `"<request_id> <результат>"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchApp {
    pub request_id: u32,
    pub package: String,
}

impl LaunchApp {
    pub fn to_packet(&self) -> Packet {
        let mut p = self.request_id.to_be_bytes().to_vec();
        p.extend_from_slice(self.package.as_bytes());
        Packet::new(PacketType::LaunchApp, p)
    }

    pub fn from_payload(data: &[u8]) -> Result<Self, DecodeError> {
        let mut c = Cursor::new(data);
        Ok(Self { request_id: c.u32()?, package: utf8(c.rest()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hello() -> Hello {
        Hello {
            version_max: 2,
            version_min: 2,
            capabilities: caps::VIDEO_JPEG | caps::ROTATION,
            role: Role::Device,
            name: "pcdock-android/1.0".into(),
        }
    }

    #[test]
    fn header_layout_is_big_endian() {
        let bytes = Packet::new(PacketType::Ping, vec![1, 2, 3]).with_flags(7).encode();
        assert_eq!(&bytes[..8], &[b'P', b'D', 0x02, 7, 0, 0, 0, 3]);
        assert_eq!(&bytes[8..], &[1, 2, 3]);
    }

    #[test]
    fn hello_roundtrip() {
        let h = sample_hello();
        assert_eq!(Hello::from_payload(&h.to_packet().payload).unwrap(), h);
    }

    #[test]
    fn display_info_roundtrip_and_validation() {
        let d = DisplayInfo {
            stream_width: 360,
            stream_height: 820,
            screen_width: 720,
            screen_height: 1640,
            density_dpi: 320,
            rotation: 3,
        };
        assert_eq!(DisplayInfo::from_payload(&d.to_packet().payload).unwrap(), d);
        let mut bad = d.to_packet().payload;
        bad[10] = 4;
        assert!(DisplayInfo::from_payload(&bad).is_err());
    }

    #[test]
    fn video_frame_roundtrip() {
        let f = VideoFrame {
            codec: codec::VIDEO_JPEG,
            key_frame: true,
            pts_us: 123_456_789,
            data: vec![0xFF, 0xD8, 1, 2],
        };
        assert_eq!(VideoFrame::from_packet(&f.to_packet()).unwrap(), f);
    }

    #[test]
    fn other_messages_roundtrip() {
        let a = AudioConfig { codec: codec::AUDIO_OPUS, sample_rate: 48_000, channels: 2 };
        assert_eq!(AudioConfig::from_payload(&a.to_packet().payload).unwrap(), a);
        let f = AudioFrame { pts_us: 9, data: vec![5, 6, 7] };
        assert_eq!(AudioFrame::from_payload(&f.to_packet().payload).unwrap(), f);
        let s = SetStream {
            video: true,
            audio: true,
            max_width: 720,
            max_height: 1280,
            jpeg_quality: 80,
            max_fps: 30,
        };
        assert_eq!(SetStream::from_payload(&s.to_packet().payload).unwrap(), s);
        let st = Status { code: 1, detail: "проекция остановлена".into() };
        assert_eq!(Status::from_payload(&st.to_packet().payload).unwrap(), st);
        let e = ErrorMsg { code: 2, message: "bad".into() };
        assert_eq!(ErrorMsg::from_payload(&e.to_packet().payload).unwrap(), e);
        assert_eq!(nonce_from_payload(&ping_packet(77).payload).unwrap(), 77);
    }

    #[test]
    fn truncated_payload_is_error() {
        assert_eq!(Hello::from_payload(&[0, 2]), Err(DecodeError::Truncated));
        assert_eq!(nonce_from_payload(&[1, 2, 3]), Err(DecodeError::Truncated));
    }

    #[test]
    fn reader_handles_split_input() {
        let bytes =
            VideoFrame { codec: 1, key_frame: true, pts_us: 1, data: vec![9; 100] }.to_packet().encode();
        let mut r = PacketReader::new();
        for chunk in bytes.chunks(7) {
            r.push(chunk);
        }
        let p = r.next_packet().expect("пакет должен собраться");
        assert_eq!(p.ptype, PacketType::VideoFrame);
        assert!(r.next_packet().is_none());
        assert_eq!(r.buffered(), 0);
    }

    #[test]
    fn reader_returns_multiple_packets() {
        let mut bytes = ping_packet(1).encode();
        bytes.extend(ping_packet(2).encode());
        let mut r = PacketReader::new();
        r.push(&bytes);
        assert_eq!(nonce_from_payload(&r.next_packet().unwrap().payload).unwrap(), 1);
        assert_eq!(nonce_from_payload(&r.next_packet().unwrap().payload).unwrap(), 2);
        assert!(r.next_packet().is_none());
    }

    #[test]
    fn reader_waits_for_incomplete_packet() {
        let bytes = ping_packet(5).encode();
        let mut r = PacketReader::new();
        r.push(&bytes[..bytes.len() - 1]);
        assert!(r.next_packet().is_none());
        r.push(&bytes[bytes.len() - 1..]);
        assert!(r.next_packet().is_some());
    }

    #[test]
    fn reader_resyncs_after_garbage() {
        let mut bytes = vec![0x13, 0x37, 0x00, b'P', 0x55];
        bytes.extend(ping_packet(10).encode());
        bytes.extend(ping_packet(11).encode());
        let mut r = PacketReader::new();
        r.push(&bytes);
        assert_eq!(nonce_from_payload(&r.next_packet().unwrap().payload).unwrap(), 10);
        assert_eq!(nonce_from_payload(&r.next_packet().unwrap().payload).unwrap(), 11);
        assert_eq!(r.dropped_bytes(), 5);
    }

    #[test]
    fn reader_rejects_fake_header_inside_garbage() {
        // Внутри мусора есть ложное "PD" с короткой «длиной», за ним нет настоящего заголовка.
        let mut bytes = vec![0u8; 3];
        bytes.extend_from_slice(&[b'P', b'D', 0x02, 0, 0, 0, 0, 1, 0xAA, 0xBB, 0xCC]);
        bytes.extend(ping_packet(42).encode());
        let mut r = PacketReader::new();
        r.push(&bytes);
        let p = r.next_packet().unwrap();
        assert_eq!(nonce_from_payload(&p.payload).unwrap(), 42);
    }

    #[test]
    fn reader_skips_oversized_length() {
        let mut bytes = vec![b'P', b'D', 0x02, 0, 0xFF, 0xFF, 0xFF, 0xFF];
        bytes.extend(ping_packet(3).encode());
        let mut r = PacketReader::new();
        r.push(&bytes);
        assert_eq!(nonce_from_payload(&r.next_packet().unwrap().payload).unwrap(), 3);
    }

    #[test]
    fn unknown_type_is_passed_through_and_can_be_skipped() {
        let mut bytes = Packet::new(PacketType::Unknown(0x33), vec![1, 2]).encode();
        bytes.extend(ping_packet(8).encode());
        let mut r = PacketReader::new();
        r.push(&bytes);
        assert_eq!(r.next_packet().unwrap().ptype, PacketType::Unknown(0x33));
        assert_eq!(r.next_packet().unwrap().ptype, PacketType::Ping);
    }

    #[test]
    fn negotiation() {
        let phone = sample_hello();
        let host =
            Hello { role: Role::Host, capabilities: caps::VIDEO_JPEG | caps::AUDIO_OPUS, ..sample_hello() };
        assert_eq!(negotiate(&host, &phone), Ok((2, caps::VIDEO_JPEG)));
        let old_phone = Hello { version_max: 1, version_min: 1, ..sample_hello() };
        assert_eq!(negotiate(&host, &old_phone), Err(error_code::UNSUPPORTED_VERSION));
    }

    #[test]
    fn ui_dump_and_exit_info_roundtrip() {
        let bytes = ui_dump_packet(0x0102_0304).encode();
        assert_eq!(&bytes[..12], &[b'P', b'D', 0x45, 0, 0, 0, 0, 4, 1, 2, 3, 4]);
        let e =
            ExitInfoReq { request_id: 9, since_ms: 1_700_000_000_123, package: "com.example.shop".into() };
        let bytes = e.to_packet().encode();
        assert_eq!(bytes[2], 0x46);
        let mut r = PacketReader::new();
        r.push(&bytes);
        let p = r.next_packet().unwrap();
        assert_eq!(p.ptype, PacketType::ExitInfo);
        assert_eq!(ExitInfoReq::from_payload(&p.payload).unwrap(), e);
        assert!(ExitInfoReq::from_payload(&[0, 0, 0, 1, 0]).is_err());
        assert_eq!(PacketType::from_u8(0x45), PacketType::UiDump);
    }

    #[test]
    fn ip_check_roundtrip_and_url_rules() {
        let q = IpCheckReq { request_id: 0x0102_0304, url: "https://pynelo.mpd-system.top/api/v1/ip".into() };
        let bytes = q.to_packet().encode();
        assert_eq!(&bytes[..12], &[b'P', b'D', 0x47, 0, 0, 0, 0, 4 + q.url.len() as u8, 1, 2, 3, 4]);
        let mut r = PacketReader::new();
        r.push(&bytes);
        let p = r.next_packet().unwrap();
        assert_eq!(p.ptype, PacketType::IpCheck);
        assert_eq!(IpCheckReq::from_payload(&p.payload).unwrap(), q);
        assert!(IpCheckReq::from_payload(&[0, 0]).is_err());
        assert_eq!(PacketType::from_u8(0x47), PacketType::IpCheck);
        assert_eq!(PacketType::IpCheck.to_u8(), 0x47);
        assert!(valid_ip_check_url("https://pynelo.mpd-system.top/api/v1/ip"));
        assert!(valid_ip_check_url("https://1.2.3.4:8443?x=1"));
        assert!(!valid_ip_check_url("http://pynelo.mpd-system.top/api/v1/ip"), "только https");
        assert!(!valid_ip_check_url("https://"), "пустой адрес");
        assert!(!valid_ip_check_url("https://user:pw@host/x"), "без user@");
        assert!(!valid_ip_check_url("https://host/a b"), "без пробелов");
        assert!(!valid_ip_check_url("https://host/\nx"), "без управляющих символов");
        assert!(!valid_ip_check_url(&format!("https://h/{}", "a".repeat(600))), "не длиннее 512");
        assert!(!valid_ip_check_url("file:///etc/passwd"));
    }

    #[test]
    fn http_local_roundtrip_and_rules() {
        let q = HttpLocalReq {
            request_id: 0x0102_0304,
            method: "POST".into(),
            url: "http://192.168.1.1/cgi/login?data=x".into(),
            headers: vec![("Content-Type".into(), "text/plain".into()), ("Cookie".into(), "a=\"b\"".into())],
            body: "line1\nline2 \\ \u{1}é".into(),
            timeout_ms: 0,
        };
        assert_eq!(q.check(), Ok(()));
        let json = concat!(
            r#"{"method":"POST","url":"http://192.168.1.1/cgi/login?data=x","#,
            r#""headers":{"Content-Type":"text/plain","Cookie":"a=\"b\""},"#,
            r#""body":"line1\nline2 \\ \u0001é","timeout_ms":10000}"#
        );
        assert_eq!(q.to_json(), json);
        let bytes = q.to_packet().encode();
        assert_eq!(bytes[2], 0x48);
        let mut r = PacketReader::new();
        r.push(&bytes);
        let p = r.next_packet().unwrap();
        assert_eq!(p.ptype, PacketType::HttpLocal);
        assert_eq!(HttpLocalReq::split_payload(&p.payload).unwrap(), (0x0102_0304, json.to_string()));
        assert!(HttpLocalReq::split_payload(&[0, 0]).is_err());
        assert_eq!(PacketType::from_u8(0x48), PacketType::HttpLocal);
        assert_eq!(PacketType::HttpLocal.to_u8(), 0x48);
        // Без заголовков и тела — только метод, URL и срок (урезанный до 30 с)
        let g = HttpLocalReq {
            method: "GET".into(),
            headers: vec![],
            body: String::new(),
            timeout_ms: 99_000,
            ..q.clone()
        };
        assert_eq!(
            g.to_json(),
            r#"{"method":"GET","url":"http://192.168.1.1/cgi/login?data=x","timeout_ms":30000}"#
        );
        assert_eq!(HttpLocalReq { timeout_ms: 2500, ..g.clone() }.effective_timeout_ms(), 2500);

        let bad = |f: &dyn Fn(&mut HttpLocalReq)| {
            let mut x = q.clone();
            f(&mut x);
            x.check()
        };
        assert_eq!(bad(&|x| x.method = "PUT".into()), Err(HttpLocalInvalid::Request));
        assert_eq!(bad(&|x| x.method = "get".into()), Err(HttpLocalInvalid::Request));
        assert_eq!(bad(&|x| x.method = "GET".into()), Err(HttpLocalInvalid::Request), "у GET нет тела");
        assert_eq!(bad(&|x| x.body = "a".repeat(HTTP_LOCAL_MAX_BODY + 1)), Err(HttpLocalInvalid::Request));
        assert_eq!(bad(&|x| x.body = "a".repeat(HTTP_LOCAL_MAX_BODY)), Ok(()));
        assert_eq!(bad(&|x| x.headers.push(("cookie".into(), "x".into()))), Err(HttpLocalInvalid::Request));
        assert_eq!(bad(&|x| x.headers.push(("Bad Name".into(), "x".into()))), Err(HttpLocalInvalid::Request));
        assert_eq!(
            bad(&|x| x.headers.push(("X".into(), "a\r\nHost: evil".into()))),
            Err(HttpLocalInvalid::Request)
        );
        assert_eq!(bad(&|x| x.headers.push(("X".into(), "a\tb".into()))), Ok(()));
        assert_eq!(bad(&|x| x.url = "http://8.8.8.8/".into()), Err(HttpLocalInvalid::Url));

        for ok in [
            "http://192.168.1.1",
            "http://192.168.1.1/",
            "https://10.0.0.1:8443/x?y=1#z",
            "http://172.16.0.1/",
            "http://172.31.255.254/",
            "http://169.254.1.2/",
            "http://192.168.0.1:80",
        ] {
            assert!(valid_http_local_url(ok), "{ok}");
        }
        for no in [
            "http://8.8.8.8/",
            "http://172.32.0.1/",
            "http://172.15.0.1/",
            "http://127.0.0.1/",
            "http://192.169.1.1/",
            "http://router.local/",
            "http://tplinkmodem.net/",
            "http://192.168.1/",
            "http://192.168.1.1.1/",
            "http://192.168.01.1/",
            "http://192.168.1.256/",
            "http://0xC0.168.1.1/",
            "http://[fe80::1]/",
            "http://user:pw@192.168.1.1/",
            "http://192.168.1.1:0/",
            "http://192.168.1.1:65536/",
            "http://192.168.1.1:/",
            "http://192.168.1.1/a b",
            "ftp://192.168.1.1/",
            "file:///etc/passwd",
            "http://",
        ] {
            assert!(!valid_http_local_url(no), "{no}");
        }
        assert!(
            !valid_http_local_url(&format!("http://192.168.1.1/{}", "a".repeat(2100))),
            "не длиннее 2048"
        );
    }

    #[test]
    fn text_input_roundtrip_and_rules() {
        let q = TextInputReq {
            request_id: 0x0A0B_0C0D,
            text: "Привет, мир! 🌍👨‍👩‍👧 #тест\nвторая строка".into(),
        };
        assert!(q.valid());
        let bytes = q.to_packet().encode();
        assert_eq!(bytes[2], 0x49);
        let mut r = PacketReader::new();
        r.push(&bytes);
        let p = r.next_packet().unwrap();
        assert_eq!(p.ptype, PacketType::TextInput);
        assert_eq!(&p.payload[..4], &[0x0A, 0x0B, 0x0C, 0x0D]);
        assert_eq!(TextInputReq::decode(&p.payload).unwrap(), q);
        assert!(TextInputReq::decode(&[0, 0]).is_err());
        assert_eq!(PacketType::from_u8(0x49), PacketType::TextInput);
        assert_eq!(PacketType::TextInput.to_u8(), 0x49);

        assert!(!valid_text_input(""));
        assert!(!valid_text_input("a\0b"));
        assert!(valid_text_input(&"я".repeat(TEXT_INPUT_MAX / 2)), "ровно предел (кириллица — 2 байта)");
        assert!(!valid_text_input(&"я".repeat(TEXT_INPUT_MAX / 2 + 1)));
        assert!(valid_text_input("\t\r\n"));
    }

    #[test]
    fn launch_app_roundtrip() {
        let l = LaunchApp { request_id: 0x0102_0304, package: "com.android.settings".into() };
        let bytes = l.to_packet().encode();
        assert_eq!(&bytes[..8], &[b'P', b'D', 0x43, 0, 0, 0, 0, 24]);
        assert_eq!(&bytes[8..12], &[1, 2, 3, 4]);
        let mut r = PacketReader::new();
        r.push(&bytes);
        let p = r.next_packet().unwrap();
        assert_eq!(p.ptype, PacketType::LaunchApp);
        assert_eq!(LaunchApp::from_payload(&p.payload).unwrap(), l);
        assert!(LaunchApp::from_payload(&[0, 1]).is_err());
        assert!(valid_package("com.example.shop_app2"));
        assert!(
            !valid_package("1com.x")
                && !valid_package("")
                && !valid_package("com.x;reboot")
                && !valid_package("com x")
        );
        assert!(!valid_package(&"a".repeat(256)));
    }
}
