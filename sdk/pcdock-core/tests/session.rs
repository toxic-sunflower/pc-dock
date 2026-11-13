//! Сценарии сессии с имитацией телефона.
use pcdock_core::hid::*;
use pcdock_core::input::*;
use pcdock_core::packet::*;
use pcdock_core::session::*;

fn phone_hello() -> Packet {
    Hello {
        version_max: 2,
        version_min: 2,
        capabilities: caps::VIDEO_JPEG | caps::ROTATION,
        role: Role::Device,
        name: "fake-phone/1".into(),
    }
    .to_packet()
}

fn display(rotation: u8, sw: u16, sh: u16) -> DisplayInfo {
    DisplayInfo {
        stream_width: sw / 2,
        stream_height: sh / 2,
        screen_width: sw,
        screen_height: sh,
        density_dpi: 320,
        rotation,
    }
}

fn drain(s: &mut Session) -> (Vec<Vec<u8>>, Vec<ControlRequest>) {
    let (mut streams, mut controls) = (Vec::new(), Vec::new());
    while let Some(t) = s.poll_transmit() {
        match t {
            Transmit::Stream(b) => streams.push(b),
            Transmit::Control(c) => controls.push(c),
        }
    }
    (streams, controls)
}

fn events(s: &mut Session) -> Vec<Event> {
    std::iter::from_fn(|| s.poll_event()).collect()
}

fn feed(s: &mut Session, now: u64, packets: &[Packet]) {
    let mut bytes = Vec::new();
    for p in packets {
        p.encode_into(&mut bytes);
    }
    s.handle_input(now, &bytes);
}

fn active_session() -> Session {
    let mut s = Session::new(Config::default());
    s.start(0);
    feed(&mut s, 10, &[phone_hello(), display(0, 720, 1640).to_packet()]);
    drain(&mut s);
    events(&mut s);
    s
}

#[test]
fn happy_path_hello_registration_and_events() {
    let mut s = Session::new(Config::default());
    assert_eq!(s.state(), State::Idle);
    s.start(0);
    assert_eq!(s.state(), State::Connecting);

    let (streams, controls) = drain(&mut s);
    assert_eq!(streams.len(), 1);
    let sent = PacketReader::new();
    let mut r = sent;
    r.push(&streams[0]);
    let hello = r.next_packet().unwrap();
    assert_eq!(hello.ptype, PacketType::Hello);
    assert_eq!(Hello::from_payload(&hello.payload).unwrap().role, Role::Host);
    // три устройства: REGISTER + SET_DESC на каждое
    assert_eq!(controls.iter().filter(|c| c.request == AOA_REGISTER_HID).count(), 3);

    // Ответ телефона приходит кусками по 5 байт
    let mut bytes = Vec::new();
    for p in [
        phone_hello(),
        display(0, 720, 1640).to_packet(),
        VideoFrame { codec: codec::VIDEO_JPEG, key_frame: true, pts_us: 5, data: vec![0xFF, 0xD8] }
            .to_packet(),
    ] {
        p.encode_into(&mut bytes);
    }
    for chunk in bytes.chunks(5) {
        s.handle_input(100, chunk);
    }
    let ev = events(&mut s);
    assert!(matches!(&ev[0], Event::Connected { version: 2, peer_name, .. } if peer_name == "fake-phone/1"));
    assert!(matches!(ev[1], Event::Display(_)));
    assert!(matches!(&ev[2], Event::Video(f) if f.pts_us == 5));
    assert_eq!(s.state(), State::Active);
    assert_eq!(s.negotiated_capabilities(), caps::VIDEO_JPEG | caps::ROTATION);
    // Размер экрана известен, абсолютный курсор работает
    assert!(s.cursor_move_abs(0.5, 0.5).is_ok());
}

#[test]
fn hello_is_retried_and_peer_silent_is_reported_once() {
    let mut s = Session::new(Config::default());
    s.start(0);
    drain(&mut s);
    assert_eq!(s.next_timeout(), Some(1000));
    s.handle_timeout(1000);
    assert_eq!(drain(&mut s).0.len(), 1, "повторный HELLO");
    s.handle_timeout(1500);
    assert!(drain(&mut s).0.is_empty(), "рано повторять");
    for t in [2000, 3000, 4000, 5000] {
        s.handle_timeout(t);
    }
    assert!(events(&mut s).is_empty());
    s.handle_timeout(6000);
    assert_eq!(events(&mut s), vec![Event::PeerSilent]);
    s.handle_timeout(7000);
    assert!(events(&mut s).is_empty(), "сообщаем один раз");
    // телефон всё-таки ответил
    feed(&mut s, 7500, &[phone_hello()]);
    assert!(matches!(events(&mut s)[0], Event::Connected { .. }));
}

#[test]
fn version_mismatch_sends_error_and_disconnects() {
    let mut s = Session::new(Config::default());
    s.start(0);
    drain(&mut s);
    let old =
        Hello { version_max: 1, version_min: 1, ..Hello::from_payload(&phone_hello().payload).unwrap() };
    feed(&mut s, 5, &[old.to_packet()]);
    let (streams, _) = drain(&mut s);
    let mut r = PacketReader::new();
    r.push(&streams[0]);
    let p = r.next_packet().unwrap();
    assert_eq!(p.ptype, PacketType::Error);
    assert_eq!(ErrorMsg::from_payload(&p.payload).unwrap().code, error_code::UNSUPPORTED_VERSION);
    assert_eq!(events(&mut s), vec![Event::Disconnected(DisconnectReason::VersionMismatch)]);
    assert_eq!(s.state(), State::Idle);
}

#[test]
fn stream_settings_are_sent_after_negotiation() {
    let settings = SetStream {
        video: true,
        audio: true,
        max_width: 720,
        max_height: 1280,
        jpeg_quality: 70,
        max_fps: 30,
    };
    let mut s = Session::new(Config { stream: Some(settings), ..Config::default() });
    s.start(0);
    drain(&mut s);
    feed(&mut s, 5, &[phone_hello()]);
    let (streams, _) = drain(&mut s);
    let mut r = PacketReader::new();
    r.push(&streams[0]);
    let p = r.next_packet().unwrap();
    assert_eq!(p.ptype, PacketType::SetStream);
    assert_eq!(SetStream::from_payload(&p.payload).unwrap(), settings);
}

#[test]
fn ping_every_interval_and_timeout_when_phone_is_silent() {
    let mut s = active_session();
    assert_eq!(s.next_timeout(), Some(2010)); // ping от момента согласования (10 мс)
    s.handle_timeout(2010);
    let (streams, _) = drain(&mut s);
    let mut r = PacketReader::new();
    r.push(&streams[0]);
    let p = r.next_packet().unwrap();
    assert_eq!(p.ptype, PacketType::Ping);
    assert_eq!(nonce_from_payload(&p.payload).unwrap(), 1);
    // телефон отвечает: таймаут отодвигается
    feed(&mut s, 3000, &[pong_packet(1)]);
    s.handle_timeout(8000);
    assert!(events(&mut s).is_empty());
    s.handle_timeout(9000);
    assert_eq!(events(&mut s), vec![Event::Disconnected(DisconnectReason::Timeout)]);
    assert_eq!(s.state(), State::Idle);
    assert_eq!(s.next_timeout(), None);
}

#[test]
fn phone_ping_is_answered_with_pong() {
    let mut s = active_session();
    feed(&mut s, 50, &[ping_packet(99)]);
    let (streams, _) = drain(&mut s);
    let mut r = PacketReader::new();
    r.push(&streams[0]);
    let p = r.next_packet().unwrap();
    assert_eq!((p.ptype, nonce_from_payload(&p.payload).unwrap()), (PacketType::Pong, 99));
}

#[test]
fn rotation_event_releases_touches_and_maps_following_ones() {
    let mut s = active_session();
    s.touch(TouchPoint::new(0, TouchPhase::Down, 0.5, 0.5)).unwrap();
    drain(&mut s);
    feed(&mut s, 100, &[display(1, 1640, 720).to_packet()]);
    let (_, controls) = drain(&mut s);
    let hid: Vec<_> = controls.iter().filter(|c| c.request == AOA_SEND_HID_EVENT).collect();
    assert_eq!(hid.len(), 1, "палец отпущен при повороте");
    assert_eq!(hid[0].data[1], 0);
    assert_eq!(s.input().rotation(), 1);
    assert_eq!(s.display().unwrap().screen_width, 1640);
    // 90°: точка картинки (0.3, 0.9) -> (0.1, 0.3) по корпусу
    s.touch(TouchPoint::new(0, TouchPhase::Down, 0.3, 0.9)).unwrap();
    let (_, controls) = drain(&mut s);
    let r = &controls[0].data;
    let (x, y) = (u16::from_le_bytes([r[3], r[4]]), u16::from_le_bytes([r[5], r[6]]));
    assert!((i32::from(x) - 3277).abs() < 40 && (i32::from(y) - 9830).abs() < 40);
}

#[test]
fn garbage_in_the_stream_does_not_break_the_session() {
    let mut s = active_session();
    let mut bytes = vec![0x00, 0x13, b'P', 0x37, 0xFF];
    VideoFrame { codec: 1, key_frame: true, pts_us: 7, data: vec![1] }.to_packet().encode_into(&mut bytes);
    VideoFrame { codec: 1, key_frame: true, pts_us: 8, data: vec![2] }.to_packet().encode_into(&mut bytes);
    s.handle_input(200, &bytes);
    let ev = events(&mut s);
    assert_eq!(ev.len(), 2);
    assert!(s.dropped_bytes() > 0);
}

#[test]
fn malformed_packet_is_reported_and_skipped() {
    let mut s = active_session();
    feed(
        &mut s,
        20,
        &[
            Packet::new(PacketType::DisplayInfo, vec![1, 2, 3]),
            Packet::new(PacketType::Unknown(0x66), vec![9]),
        ],
    );
    let ev = events(&mut s);
    assert_eq!(ev.len(), 1);
    assert!(matches!(&ev[0], Event::Malformed(m) if m.contains("DISPLAY_INFO")));
}

#[test]
fn peer_error_closes_the_session() {
    let mut s = active_session();
    feed(&mut s, 30, &[ErrorMsg { code: error_code::INTERNAL, message: "oops".into() }.to_packet()]);
    let ev = events(&mut s);
    assert!(ev
        .iter()
        .any(|e| matches!(e, Event::Disconnected(DisconnectReason::PeerError(m)) if m.message == "oops")));
    assert_eq!(s.state(), State::Idle);
}

#[test]
fn reconnect_registers_devices_again_and_forgets_old_input_state() {
    let mut s = active_session();
    s.key(4, true).unwrap();
    s.close();
    assert_eq!(events(&mut s), vec![Event::Disconnected(DisconnectReason::Closed)]);
    assert_eq!(
        s.key(4, true),
        Err(InputError::DeviceDisabled(DeviceKind::Keyboard)),
        "после потери устройства ввод недоступен"
    );
    drain(&mut s);
    s.start(5000);
    let (_, controls) = drain(&mut s);
    assert_eq!(controls.iter().filter(|c| c.request == AOA_REGISTER_HID).count(), 3);
    feed(&mut s, 5010, &[phone_hello()]);
    s.key(4, true).unwrap();
    let (_, controls) = drain(&mut s);
    assert_eq!(controls[0].data, vec![0, 0, 4, 0, 0, 0, 0, 0], "прежняя нажатая клавиша не «залипла»");
}

#[test]
fn device_enable_command_unregisters_and_registers() {
    let mut s = active_session();
    s.device_enable(DeviceKind::Mouse, false);
    let (_, controls) = drain(&mut s);
    assert_eq!(controls.last().unwrap().request, AOA_UNREGISTER_HID);
    assert_eq!(s.cursor_move_abs(0.1, 0.1), Err(InputError::DeviceDisabled(DeviceKind::Mouse)));
    s.device_enable(DeviceKind::Mouse, true);
    let (_, controls) = drain(&mut s);
    assert_eq!(controls[0].request, AOA_REGISTER_HID);
    assert!(s.cursor_move_abs(0.1, 0.1).is_ok());
}

#[test]
fn commands_do_nothing_for_stream_when_not_active() {
    let mut s = Session::new(Config::default());
    s.request_frame();
    s.set_stream(SetStream::default());
    assert!(drain(&mut s).0.is_empty());
    let mut s = active_session();
    s.request_frame();
    assert_eq!(drain(&mut s).0.len(), 1);
}

#[test]
fn data_is_ignored_when_session_is_not_started() {
    let mut s = Session::new(Config::default());
    feed(&mut s, 0, &[phone_hello()]);
    assert!(events(&mut s).is_empty());
}

#[test]
fn launch_app_only_when_both_sides_agree() {
    // Старый телефон (без бита LAUNCH_APP) — команда не уходит
    let mut s = active_session();
    assert_eq!(s.launch_app(1, "com.android.settings"), Err(LaunchError::Unsupported));
    assert!(drain(&mut s).0.is_empty());

    // Хост и телефон умеют — уходит LAUNCH_APP, ответ STATUS 18 доходит событием
    let cfg = Config { capabilities: Config::default().capabilities | caps::LAUNCH_APP, ..Config::default() };
    let mut s = Session::new(cfg);
    assert_eq!(s.launch_app(1, "com.android.settings"), Err(LaunchError::NotActive));
    s.start(0);
    let hello = Hello {
        version_max: 2,
        version_min: 2,
        capabilities: caps::VIDEO_JPEG | caps::LAUNCH_APP,
        role: Role::Device,
        name: "fake-phone/2".into(),
    };
    feed(&mut s, 10, &[hello.to_packet()]);
    drain(&mut s);
    events(&mut s);
    assert_eq!(s.launch_app(7, "com.x;reboot"), Err(LaunchError::BadPackage));
    assert_eq!(s.launch_app(7, "com.android.settings"), Ok(()));
    let (streams, _) = drain(&mut s);
    assert_eq!(streams.len(), 1);
    let mut r = PacketReader::new();
    r.push(&streams[0]);
    let p = r.next_packet().unwrap();
    assert_eq!(p.ptype, PacketType::LaunchApp);
    assert_eq!(
        LaunchApp::from_payload(&p.payload).unwrap(),
        LaunchApp { request_id: 7, package: "com.android.settings".into() }
    );
    feed(&mut s, 20, &[Status { code: 18, detail: "7 ok".into() }.to_packet()]);
    assert!(events(&mut s).contains(&Event::Status(Status { code: 18, detail: "7 ok".into() })));
    // Список приложений этот телефон не согласовал
    assert_eq!(s.list_apps(8), Err(LaunchError::Unsupported));
}

#[test]
fn list_apps_request_when_agreed() {
    let cfg = Config { capabilities: Config::default().capabilities | caps::LIST_APPS, ..Config::default() };
    let mut s = Session::new(cfg);
    assert_eq!(s.list_apps(1), Err(LaunchError::NotActive));
    s.start(0);
    let hello = Hello {
        version_max: 2,
        version_min: 2,
        capabilities: caps::VIDEO_JPEG | caps::LIST_APPS,
        role: Role::Device,
        name: "p".into(),
    };
    feed(&mut s, 10, &[hello.to_packet()]);
    drain(&mut s);
    assert_eq!(s.list_apps(0x0A0B_0C0D), Ok(()));
    let (streams, _) = drain(&mut s);
    assert_eq!(streams, vec![vec![b'P', b'D', 0x44, 0, 0, 0, 0, 4, 0x0A, 0x0B, 0x0C, 0x0D]]);
}

#[test]
fn ui_dump_and_exit_info_only_when_agreed() {
    // Старый телефон (без битов 9 и 10) — команды не уходят
    let mut s = active_session();
    assert_eq!(s.ui_dump(1), Err(LaunchError::Unsupported));
    assert_eq!(s.exit_info(1, 0, "com.example.shop"), Err(LaunchError::Unsupported));
    assert!(drain(&mut s).0.is_empty());

    let cfg = Config {
        capabilities: Config::default().capabilities | caps::UI_TREE | caps::EXIT_INFO,
        ..Config::default()
    };
    let mut s = Session::new(cfg);
    assert_eq!(s.ui_dump(1), Err(LaunchError::NotActive));
    s.start(0);
    let hello = Hello {
        version_max: 2,
        version_min: 2,
        capabilities: caps::VIDEO_JPEG | caps::UI_TREE | caps::EXIT_INFO,
        role: Role::Device,
        name: "p".into(),
    };
    feed(&mut s, 10, &[hello.to_packet()]);
    drain(&mut s);
    events(&mut s);
    assert_eq!(s.ui_dump(5), Ok(()));
    assert_eq!(s.exit_info(6, 1_700_000_000_000, "com.x;reboot"), Err(LaunchError::BadPackage));
    assert_eq!(s.exit_info(6, 1_700_000_000_000, "com.example.shop"), Ok(()));
    let (streams, _) = drain(&mut s);
    assert_eq!(streams.len(), 2);
    assert_eq!(streams[0], vec![b'P', b'D', 0x45, 0, 0, 0, 0, 4, 0, 0, 0, 5]);
    let mut r = PacketReader::new();
    r.push(&streams[1]);
    let p = r.next_packet().unwrap();
    assert_eq!(p.ptype, PacketType::ExitInfo);
    assert_eq!(
        ExitInfoReq::from_payload(&p.payload).unwrap(),
        ExitInfoReq { request_id: 6, since_ms: 1_700_000_000_000, package: "com.example.shop".into() }
    );
    // Ответы — обычные STATUS
    feed(&mut s, 20, &[Status { code: 20, detail: "5 1/1 no_service".into() }.to_packet()]);
    assert!(events(&mut s).contains(&Event::Status(Status { code: 20, detail: "5 1/1 no_service".into() })));
}

#[test]
fn ip_check_only_when_agreed() {
    // Старый телефон (без бита 11) — команда не уходит
    let mut s = active_session();
    assert_eq!(s.ip_check(1, "https://pynelo.mpd-system.top/api/v1/ip"), Err(LaunchError::Unsupported));
    assert!(drain(&mut s).0.is_empty());

    let cfg = Config { capabilities: Config::default().capabilities | caps::IP_CHECK, ..Config::default() };
    let mut s = Session::new(cfg);
    assert_eq!(s.ip_check(1, "https://x.y/ip"), Err(LaunchError::NotActive));
    s.start(0);
    let hello = Hello {
        version_max: 2,
        version_min: 2,
        capabilities: caps::VIDEO_JPEG | caps::IP_CHECK,
        role: Role::Device,
        name: "p".into(),
    };
    feed(&mut s, 10, &[hello.to_packet()]);
    drain(&mut s);
    events(&mut s);
    assert_eq!(s.ip_check(2, "http://x.y/ip"), Err(LaunchError::BadUrl));
    assert_eq!(s.ip_check(2, "https://a@x.y/ip"), Err(LaunchError::BadUrl));
    assert!(drain(&mut s).0.is_empty(), "неверный URL не уходит");
    assert_eq!(s.ip_check(7, "https://x.y/ip"), Ok(()));
    let (streams, _) = drain(&mut s);
    assert_eq!(streams.len(), 1);
    let mut r = PacketReader::new();
    r.push(&streams[0]);
    let p = r.next_packet().unwrap();
    assert_eq!(p.ptype, PacketType::IpCheck);
    assert_eq!(
        IpCheckReq::from_payload(&p.payload).unwrap(),
        IpCheckReq { request_id: 7, url: "https://x.y/ip".into() }
    );
    // Ответ — обычный STATUS 23
    let d = r#"7 {"ok":true,"code":200,"body":"{\"ip\":\"1.2.3.4\"}","transport":"cellular","ms":120}"#;
    feed(&mut s, 20, &[Status { code: 23, detail: d.into() }.to_packet()]);
    assert!(events(&mut s).contains(&Event::Status(Status { code: 23, detail: d.into() })));
}

#[test]
fn http_local_only_when_agreed() {
    use pcdock_core::link::AppRequest;
    let req = |request: u32, url: &str| AppRequest::HttpLocal {
        request,
        method: "POST".into(),
        url: url.into(),
        headers: vec![("Content-Type".into(), "text/plain".into())],
        body: "[cgi]".into(),
        timeout_ms: 0,
    };
    // Хост просит бит 12 по умолчанию
    assert_ne!(Config::default().capabilities & caps::HTTP_LOCAL, 0);

    // Старый телефон (без бита 12) — команда не уходит, ответ «unsupported» кладёт сама сессия
    let mut s = active_session();
    s.app_request(&req(3, "http://192.168.1.1/cgi"));
    assert!(drain(&mut s).0.is_empty());
    assert!(
        events(&mut s).contains(&Event::Status(Status { code: 24, detail: "3 error unsupported".into() }))
    );

    let mut s = Session::new(Config::default());
    s.start(0);
    let hello = Hello {
        version_max: 2,
        version_min: 2,
        capabilities: caps::VIDEO_JPEG | caps::HTTP_LOCAL,
        role: Role::Device,
        name: "p".into(),
    };
    feed(&mut s, 10, &[hello.to_packet()]);
    drain(&mut s);
    events(&mut s);

    // Адрес не из локальной сети и неверный метод — не уходят
    s.app_request(&req(4, "http://8.8.8.8/"));
    s.app_request(&AppRequest::HttpLocal {
        request: 5,
        method: "DELETE".into(),
        url: "http://192.168.1.1/".into(),
        headers: vec![],
        body: String::new(),
        timeout_ms: 0,
    });
    assert!(drain(&mut s).0.is_empty(), "неверный запрос не уходит");
    let ev = events(&mut s);
    assert!(ev.contains(&Event::Status(Status { code: 24, detail: "4 error invalid_url".into() })), "{ev:?}");
    assert!(
        ev.contains(&Event::Status(Status { code: 24, detail: "5 error invalid_request".into() })),
        "{ev:?}"
    );

    s.app_request(&req(7, "http://192.168.1.1/cgi?1"));
    let (streams, _) = drain(&mut s);
    assert_eq!(streams.len(), 1);
    let mut r = PacketReader::new();
    r.push(&streams[0]);
    let p = r.next_packet().unwrap();
    assert_eq!(p.ptype, PacketType::HttpLocal);
    assert_eq!(
        HttpLocalReq::split_payload(&p.payload).unwrap(),
        (
            7,
            r#"{"method":"POST","url":"http://192.168.1.1/cgi?1","headers":{"Content-Type":"text/plain"},"body":"[cgi]","timeout_ms":10000}"#
                .to_string()
        )
    );
    // Ответ — обычный STATUS 24 (401 — тоже успех: роутер так отвечает на вход)
    let d = r#"7 {"ok":true,"code":401,"body":"[error]0","headers":{"content-type":"text/plain"},"ms":35}"#;
    feed(&mut s, 20, &[Status { code: 24, detail: d.into() }.to_packet()]);
    assert!(events(&mut s).contains(&Event::Status(Status { code: 24, detail: d.into() })));
}

#[test]
fn text_input_only_when_agreed() {
    use pcdock_core::link::AppRequest;
    let req = |request: u32, text: &str| AppRequest::TypeText { request, text: text.into() };
    // Хост просит бит 13 по умолчанию
    assert_ne!(Config::default().capabilities & caps::TEXT_INPUT, 0);

    // Старый телефон (без бита 13) — команда не уходит, ответ «unsupported» кладёт сама сессия
    let mut s = active_session();
    s.app_request(&req(3, "Привет"));
    assert!(drain(&mut s).0.is_empty());
    assert!(
        events(&mut s).contains(&Event::Status(Status { code: 25, detail: "3 error unsupported".into() }))
    );

    let mut s = Session::new(Config::default());
    s.start(0);
    let hello = Hello {
        version_max: 2,
        version_min: 2,
        capabilities: caps::VIDEO_JPEG | caps::TEXT_INPUT,
        role: Role::Device,
        name: "p".into(),
    };
    feed(&mut s, 10, &[hello.to_packet()]);
    drain(&mut s);
    events(&mut s);

    // Пустой и слишком длинный текст — не уходят
    s.app_request(&req(4, ""));
    s.app_request(&req(5, &"я".repeat(TEXT_INPUT_MAX)));
    assert!(drain(&mut s).0.is_empty(), "неверный текст не уходит");
    let ev = events(&mut s);
    assert!(
        ev.contains(&Event::Status(Status { code: 25, detail: "4 error invalid_request".into() })),
        "{ev:?}"
    );
    assert!(
        ev.contains(&Event::Status(Status { code: 25, detail: "5 error invalid_request".into() })),
        "{ev:?}"
    );

    let text = "Подпись ✨ #тест\nвторая строка 👨‍👩‍👧";
    s.app_request(&req(7, text));
    let (streams, _) = drain(&mut s);
    assert_eq!(streams.len(), 1);
    let mut r = PacketReader::new();
    r.push(&streams[0]);
    let p = r.next_packet().unwrap();
    assert_eq!(p.ptype, PacketType::TextInput);
    assert_eq!(TextInputReq::decode(&p.payload).unwrap(), TextInputReq { request_id: 7, text: text.into() });
    // Ответ — обычный STATUS 25
    let d = r#"7 {"ok":true,"method":"paste","ms":40}"#;
    feed(&mut s, 20, &[Status { code: 25, detail: d.into() }.to_packet()]);
    assert!(events(&mut s).contains(&Event::Status(Status { code: 25, detail: d.into() })));
}

#[test]
fn stream_switches_keep_size_and_quality() {
    let mut s = Session::new(Config::default());
    let base = SetStream {
        video: false,
        audio: false,
        max_width: 1280,
        max_height: 1280,
        jpeg_quality: 80,
        max_fps: 30,
    };
    s.set_initial_stream(base);
    s.start(0);
    feed(&mut s, 10, &[phone_hello(), display(0, 720, 1640).to_packet()]);
    drain(&mut s);
    s.set_stream_switches(true, false, 10);
    let (streams, _) = drain(&mut s);
    let mut r = PacketReader::new();
    r.push(&streams.concat());
    let p = r.next_packet().unwrap();
    assert_eq!(p.ptype, PacketType::SetStream);
    let st = SetStream::from_payload(&p.payload).unwrap();
    assert_eq!(
        (st.video, st.max_width, st.max_height, st.jpeg_quality, st.max_fps),
        (true, 1280, 1280, 80, 10)
    );
}
