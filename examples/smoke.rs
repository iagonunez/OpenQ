//! Headless end-to-end check of the real media backends (runs with master muted).
//! `cargo run --release --example smoke`

use openq::media::{Media, Prober};
use openq::model::{AudioCue, ContinueMode, Cue, CueKind, VideoCue, Workspace};
use openq::show::Show;
use std::time::{Duration, Instant};

fn write_wav(path: &std::path::Path, secs: f64, rate: u32) {
    let n = (secs * rate as f64) as u32;
    let mut b = Vec::new();
    let data_len = n * 4;
    b.extend(b"RIFF");
    b.extend((36 + data_len).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(rate.to_le_bytes());
    b.extend((rate * 4).to_le_bytes());
    b.extend(4u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(data_len.to_le_bytes());
    for i in 0..n {
        let s = ((i as f32 / rate as f32 * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        b.extend(s.to_le_bytes());
        b.extend(s.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

fn main() {
    env_logger::init();
    let dir = std::env::temp_dir().join("openq-smoke");
    std::fs::create_dir_all(&dir).unwrap();
    // 44.1k file: exercises the resampler if the device runs at 48k.
    let wav_a = dir.join("a.wav");
    let wav_b = dir.join("b.wav");
    write_wav(&wav_a, 1.0, 44_100);
    write_wav(&wav_b, 0.5, 48_000);

    let prober = Prober::new();
    let mut show = Show::new(prober.clone());
    let audio = show.media.audio.as_mut().expect("audio output");
    println!("device: {} {} Hz {} ch", audio.device_name, audio.rate, audio.channels);
    audio.set_master(openq::model::MIN_DB); // silent

    let mut a = Cue::new(CueKind::Audio(AudioCue { path: wav_a.clone(), ..Default::default() }));
    a.number = "1".into();
    a.continue_mode = ContinueMode::AutoFollow;
    // Varispeed 2x + EQ + custom routing (L/R swapped): 0.5 s file trimmed to 0.4 s → 0.2 s.
    let mut eq = openq::model::Eq { enabled: true, ..Default::default() };
    eq.bands[2].gain_db = 6.0;
    let mut b = Cue::new(CueKind::Audio(AudioCue {
        path: wav_b.clone(),
        start_time: 0.1,
        rate: 2.0,
        eq,
        routing: Some(vec![vec![-60.0, 0.0], vec![0.0, -60.0]]),
        ..Default::default()
    }));
    b.continue_mode = ContinueMode::AutoFollow;
    b.number = "2".into();
    let mut c = Cue::new(CueKind::Memo);
    c.number = "2.5".into();
    let (ai, bi, ci) = (a.id, b.id, c.id);
    let mut ws = Workspace::default();
    ws.cue_list.cues = vec![a, b, c];

    let video_path = dir.join(std::env::var("OPENQ_SMOKE_VIDEO").unwrap_or("v.mp4".into()));
    let have_video = std::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=duration=1:size=320x240:rate=25", "-an"])
        .args(if video_path.extension().is_some_and(|e| e == "ivf") { vec!["-c:v", "libvpx", "-f", "ivf"] } else { vec![] })
        .arg(&video_path)
        .status()
        .is_ok_and(|s| s.success());
    let mut vi = None;
    if have_video {
        // End-trim at 0.6 s of a 1 s clip.
        let mut v = Cue::new(CueKind::Video(VideoCue { path: video_path.clone(), end_time: Some(0.6), ..Default::default() }));
        v.number = "3".into();
        vi = Some(v.id);
        ws.cue_list.cues.push(v);
    }

    show.replace_workspace(ws, None);
    show.ws.settings.master_volume_db = openq::model::MIN_DB;
    show.media.audio.as_mut().unwrap().set_master(openq::model::MIN_DB);
    // Let preloading settle.
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        show.tick(Instant::now());
        std::thread::sleep(Duration::from_millis(1));
    }

    let t0 = Instant::now();
    show.go();
    let mut b_seen_at = None;
    let mut a_done_at = None;
    let mut c_seen_at = None;
    while t0.elapsed() < Duration::from_secs(3) {
        show.tick(Instant::now());
        if b_seen_at.is_none() && show.engine.instances.contains_key(&bi) {
            b_seen_at = Some(t0.elapsed());
        }
        if c_seen_at.is_none() && show.engine.instances.contains_key(&ci) {
            c_seen_at = Some(t0.elapsed());
        }
        if a_done_at.is_none() && b_seen_at.is_some() && !show.engine.instances.contains_key(&ai) {
            a_done_at = Some(t0.elapsed());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let a_info = prober.get_blocking(&wav_a).unwrap();
    println!("a.wav probe: {:?}", a_info);
    println!("cue 2 fired at {:?} (expected ≈1.0s minus lookahead)", b_seen_at);
    println!("cue 1 finished at {:?}", a_done_at);
    println!("engine log: {:?}", show.engine.log);
    assert!(b_seen_at.is_some(), "auto-follow did not fire");
    let fired = b_seen_at.unwrap().as_secs_f64();
    assert!((0.95..1.05).contains(&fired), "auto-follow timing off: {fired}");
    println!("memo after 2x cue fired at {:?} (expected ≈1.2s)", c_seen_at);
    let c_at = c_seen_at.expect("auto-follow after varispeed cue").as_secs_f64();
    assert!((1.15..1.25).contains(&c_at), "varispeed duration off: {c_at}");
    assert!(show.engine.instances.is_empty(), "cues still running after 3s");

    if let Some(vi) = vi {
        // GO on the video cue and wait for frames.
        show.engine.set_playhead(Some(vi));
        show.go();
        let t1 = Instant::now();
        let mut frames = 0u64;
        while t1.elapsed() < Duration::from_secs(3) {
            show.tick(Instant::now());
            if let Some(l) = show.media.stage_layers().first() {
                frames = frames.max(l.frame.lock().generation);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        println!("video frames decoded: {frames} (end trim 0.6 s); running after 3s: {}", show.engine.instances.contains_key(&vi));
        assert!((10..=18).contains(&frames), "end trim not honoured: {frames} frames");
        assert!(!show.engine.instances.contains_key(&vi), "video didn't finish");
    }
    // Still image with a 0.5 s duration.
    let png = dir.join("still.png");
    image::RgbaImage::from_pixel(64, 32, image::Rgba([255, 0, 0, 255])).save(&png).unwrap();
    let mut still = Cue::new(CueKind::Video(VideoCue { path: png, still_duration: 0.5, ..Default::default() }));
    still.number = "9".into();
    let si = still.id;
    show.ws.cue_list.cues.push(still);
    show.edited();
    show.engine.set_playhead(Some(si));
    show.go();
    let t2 = Instant::now();
    let mut shown = false;
    let mut gone_at = None;
    while t2.elapsed() < Duration::from_secs(2) {
        show.tick(Instant::now());
        if show.media.stage_layers().iter().any(|l| l.id == si && l.frame.lock().width == 64) {
            shown = true;
        }
        if gone_at.is_none() && !show.engine.instances.contains_key(&si) {
            gone_at = Some(t2.elapsed());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("still shown: {shown}, ended at {gone_at:?} (expected ≈0.5s)");
    assert!(shown && gone_at.is_some_and(|t| (0.45..0.6).contains(&t.as_secs_f64())));
    // --- Slices: [0, 0.2) loops until devamped, then the rest plays once.
    let mut sl = Cue::new(CueKind::Audio(AudioCue {
        path: wav_a.clone(),
        slices: vec![openq::model::SliceMarker { time: 0.2, plays: 0 }],
        ..Default::default()
    }));
    sl.number = "10".into();
    let sli = sl.id;
    show.ws.cue_list.cues.push(sl);
    show.edited();
    show.engine.set_playhead(Some(sli));
    show.go();
    let run = |show: &mut Show, secs: f64| {
        let t = Instant::now();
        while t.elapsed().as_secs_f64() < secs {
            show.tick(Instant::now());
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    run(&mut show, 1.5);
    assert!(show.engine.instances.contains_key(&sli), "vamping cue ended on its own");
    {
        use openq::engine::Backend;
        assert!(show.media.devamp(sli));
    }
    let t3 = Instant::now();
    while show.engine.instances.contains_key(&sli) && t3.elapsed() < Duration::from_secs(5) {
        show.tick(Instant::now());
        std::thread::sleep(Duration::from_millis(2));
    }
    let after = t3.elapsed().as_secs_f64();
    println!("devamp: cue ended {after:.3}s later (expected 0.8–1.4 s: rest of slice + 0.8 s tail)");
    assert!((0.75..1.45).contains(&after), "devamp timing off: {after}");

    // --- Movies with soundtracks in common containers, audio through the mixer.
    let formats: [(&str, &[&str]); 4] = [
        ("av.asf", &["-c:v", "wmv2", "-c:a", "wmav2"]),
        ("av.mov", &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"]),
        ("av.mkv", &["-c:v", "libvpx-vp9", "-c:a", "libopus"]),
        ("av.webm", &["-c:v", "libvpx", "-c:a", "libvorbis"]),
    ];
    for (i, (name, codec)) in formats.iter().enumerate() {
        let file = dir.join(name);
        let ok = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=duration=2:size=160x120:rate=25"])
            .args(["-f", "lavfi", "-i", "sine=frequency=330:duration=2"])
            .args(*codec)
            .arg(&file)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            println!("{name}: ffmpeg can't encode this format here, skipped");
            continue;
        }
        let _ = prober.get_blocking(&file);
        let mut av = Cue::new(CueKind::Video(VideoCue { path: file.clone(), ..Default::default() }));
        av.number = format!("av{i}");
        let avi = av.id;
        show.ws.cue_list.cues.push(av);
        show.edited();
        show.engine.set_playhead(Some(avi));
        show.go();
        let t = Instant::now();
        let mut frames = 0u64;
        while t.elapsed() < Duration::from_millis(1200) {
            show.tick(Instant::now());
            if let Some(l) = show.media.stage_layers().iter().find(|l| l.id == avi) {
                frames = frames.max(l.frame.lock().generation);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let played = show.media.audio.as_ref().unwrap().elapsed(avi).unwrap_or(0.0);
        println!("{name}: {frames} frames and {played:.2}s of soundtrack after 1.2 s; log {:?}", show.engine.log);
        assert!(frames >= 20 && played > 0.6, "{name} didn't play");
        run(&mut show, 1.5);
        assert!(!show.engine.instances.contains_key(&avi), "{name} didn't finish");
    }

    // --- OSC remote control.
    let mut w = Cue::new(CueKind::Wait { duration: 1.0 });
    w.number = "osc".into();
    let wi = w.id;
    show.ws.cue_list.cues.push(w);
    show.edited();
    let shared = std::sync::Arc::new(parking_lot::Mutex::new(show));
    let server = openq::osc::OscServer::start(53999, shared.clone(), || {}).expect("bind OSC");
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let msg = rosc::OscPacket::Message(rosc::OscMessage { addr: "/cue/osc/start".into(), args: vec![] });
    sock.send_to(&rosc::encoder::encode(&msg).unwrap(), "127.0.0.1:53999").unwrap();
    let t4 = Instant::now();
    let mut seen = false;
    while t4.elapsed() < Duration::from_secs(2) && !seen {
        let mut s = shared.lock();
        s.tick(Instant::now());
        seen = s.engine.instances.contains_key(&wi);
        drop(s);
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("OSC /cue/osc/start fired the cue: {seen}");
    assert!(seen);
    drop(server);
    shared.lock().shutdown();
    let _: Option<&Media> = None;
    println!("SMOKE OK");
}
