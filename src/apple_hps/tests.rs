use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use flate2::{Compression, write::ZlibEncoder};
use plist::{Dictionary, Value};
use rsa::{Pkcs1v15Encrypt, RsaPrivateKey, pkcs8::EncodePublicKey};
use serde_json::json;
use uuid::Uuid;

use crate::{
    audit::Audit,
    auth::Credentials,
    credentials::CredentialSource,
    input::Command,
    profiles::{AuthMode, Profile},
    session::{Event, Request, Session},
    streaming::{Encoding, StreamSettings},
};

use super::crypto::{Records, SrtpSender};

const SAMPLE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "\\crates\\native-hevc\\tests\\data\\pattern444.hevc"
));

fn read_u32(stream: &mut impl Read) -> u32 {
    let mut bytes = [0; 4];
    stream.read_exact(&mut bytes).unwrap();
    u32::from_be_bytes(bytes)
}

fn read_record(stream: &mut TcpStream, cipher: &mut Records) -> Vec<u8> {
    let mut length = [0; 2];
    stream.read_exact(&mut length).unwrap();
    let mut data = vec![0; usize::from(u16::from_be_bytes(length))];
    stream.read_exact(&mut data).unwrap();
    cipher.decrypt(&data).unwrap().to_vec()
}

fn nals(data: &[u8]) -> Vec<&[u8]> {
    let mut offsets = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        let length = if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        offsets.push((index, index + length));
        index += length;
    }
    offsets
        .iter()
        .enumerate()
        .map(|(index, (_, begin))| {
            &data[*begin..offsets.get(index + 1).map_or(data.len(), |(end, _)| *end)]
        })
        .collect()
}

fn media_payloads() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for (don, nal) in nals(SAMPLE).into_iter().enumerate() {
        assert!(nal.len() >= 2);
        if nal.len() < 1000 {
            let mut payload = nal[..2].to_vec();
            payload.extend_from_slice(&(don as u16).to_be_bytes());
            payload.extend_from_slice(&nal[2..]);
            out.push(payload);
        } else {
            let chunks: Vec<_> = nal[2..].chunks(900).collect();
            for (index, chunk) in chunks.iter().enumerate() {
                let mut payload = vec![
                    (nal[0] & 0x81) | (49 << 1),
                    nal[1],
                    ((nal[0] >> 1) & 63)
                        | if index == 0 { 128 } else { 0 }
                        | if index + 1 == chunks.len() { 64 } else { 0 },
                ];
                payload.extend_from_slice(&(don as u16).to_be_bytes());
                payload.extend_from_slice(chunk);
                out.push(payload);
            }
        }
    }
    out
}

fn media_answer() -> Vec<u8> {
    let mut encoded = ZlibEncoder::new(Vec::new(), Compression::default());
    encoded
        .write_all(&[0x2a, 6, 0x20, 64, 0x28, 48, 0x30, 1])
        .unwrap();
    let mut dictionary = Dictionary::new();
    dictionary.insert(
        "avcMediaStreamNegotiatorMediaBlob".into(),
        Value::Data(encoded.finish().unwrap()),
    );
    let mut plist = Vec::new();
    Value::Dictionary(dictionary)
        .to_writer_binary(&mut plist)
        .unwrap();
    let mut combined = super::offers::Offers::new(false).unwrap().audio;
    combined.extend_from_slice(&[17; 92]);
    combined.extend_from_slice(&plist);
    plist = combined;
    let mut message = vec![0, 0, 0, 2];
    message.extend_from_slice(&[0; 8]);
    message.extend_from_slice(&1105_i32.to_be_bytes());
    message.extend_from_slice(&20_u16.to_be_bytes());
    let mut layout = vec![0; 20];
    layout[..2].copy_from_slice(&1_u16.to_be_bytes());
    layout[2..4].copy_from_slice(&32_u16.to_be_bytes());
    layout[4..6].copy_from_slice(&24_u16.to_be_bytes());
    layout[6..8].copy_from_slice(&64_u16.to_be_bytes());
    layout[8..10].copy_from_slice(&48_u16.to_be_bytes());
    message.extend_from_slice(&layout);
    message.extend_from_slice(&[0; 8]);
    message.extend_from_slice(&1010_i32.to_be_bytes());
    message.extend_from_slice(&(plist.len() as u16).to_be_bytes());
    message.extend_from_slice(&plist);
    message
}

#[test]
fn encrypted_apple_peer_decodes_real_hevc_and_preserves_input_and_hello_gates() {
    let directory = tempfile::tempdir().unwrap();
    let mut audit = Audit::start(directory.path().join("audit")).unwrap();
    let listener = TcpListener::bind(("127.0.0.2", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(port < 65535);
    let video_socket = UdpSocket::bind(("127.0.0.2", port + 1)).unwrap();
    let control_socket = UdpSocket::bind(("127.0.0.2", port)).unwrap();
    let (observed, received) = mpsc::channel();
    let private = RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
    let public = private.to_public_key().to_public_key_der().unwrap();
    let server = thread::spawn(move || {
        let (mut socket, peer) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        socket.write_all(b"RFB 003.889\n").unwrap();
        let mut banner = [0; 12];
        socket.read_exact(&mut banner).unwrap();
        assert_eq!(&banner, b"RFB 003.889\n");
        socket.write_all(&[1, 33]).unwrap();
        let mut initial = [0; 15];
        socket.read_exact(&mut initial).unwrap();
        assert_eq!(
            &initial,
            b"\x21\x00\x00\x00\x0a\x01\x00RSA1\x00\x00\x00\x00"
        );
        let mut key_packet = vec![0, 0];
        key_packet.extend_from_slice(&(public.as_ref().len() as u32).to_be_bytes());
        key_packet.extend_from_slice(public.as_ref());
        socket
            .write_all(&(key_packet.len() as u32).to_be_bytes())
            .unwrap();
        socket.write_all(&key_packet).unwrap();
        let size = read_u32(&mut socket) as usize;
        assert_eq!(size, 394);
        let mut response = vec![0; size];
        socket.read_exact(&mut response).unwrap();
        let shared = private.decrypt(Pkcs1v15Encrypt, &response[138..]).unwrap();
        let wrap: [u8; 16] = shared.try_into().unwrap();
        let mut plaintext = response[8..136].to_vec();
        let aes = aes::Aes128::new((&wrap).into());
        for block in plaintext.as_chunks_mut::<16>().0 {
            aes.decrypt_block(block.into());
        }
        assert_eq!(
            plaintext[..64].split(|byte| *byte == 0).next().unwrap(),
            b"fixture"
        );
        assert_eq!(
            plaintext[64..].split(|byte| *byte == 0).next().unwrap(),
            b"FixtureOnly"
        );
        socket.write_all(&[0; 8]).unwrap();
        let mut init = [0];
        socket.read_exact(&mut init).unwrap();
        assert_eq!(init, [0xc1]);
        let mut info = Vec::new();
        info.extend_from_slice(&64_u16.to_be_bytes());
        info.extend_from_slice(&48_u16.to_be_bytes());
        info.extend_from_slice(&crate::protocol::PIXEL_FORMAT);
        let name = "HEVC \u{2019} fixture";
        let mut extended_name = vec![0; 22];
        extended_name[5..11].copy_from_slice(&[0x02, 0x80, 0xff, 0x81, 0x7f, 0xfe]);
        extended_name.extend_from_slice(name.as_bytes());
        info.extend_from_slice(&(extended_name.len() as u32).to_be_bytes());
        info.extend_from_slice(&extended_name);
        socket.write_all(&info).unwrap();
        let mut prelude = [0; 78];
        socket.read_exact(&mut prelude).unwrap();
        assert_eq!(&prelude[..4], &[0x21, 0, 0, 62]);
        let mut encodings = [0; 4];
        socket.read_exact(&mut encodings).unwrap();
        let mut methods =
            vec![0; usize::from(u16::from_be_bytes([encodings[2], encodings[3]])) * 4];
        socket.read_exact(&mut methods).unwrap();
        let mut rekey = [0; 36];
        rekey[3] = 1;
        rekey[4..20].fill(21);
        rekey[20..36].fill(31);
        for block in rekey[4..].as_chunks_mut::<16>().0 {
            aes.encrypt_block(block.into());
        }
        let mut record = vec![0, 0, 0, 1];
        record.extend_from_slice(&[0; 8]);
        record.extend_from_slice(&1103_i32.to_be_bytes());
        record.extend_from_slice(&rekey);
        socket.write_all(&record).unwrap();
        let mut cipher = Records::new(&wrap, &rekey).unwrap();
        let mut toggle = [0; 8];
        socket.read_exact(&mut toggle).unwrap();
        assert_eq!(toggle, [0x12, 0, 0, 2, 0, 1, 0, 0]);
        assert_eq!(read_record(&mut socket, &mut cipher)[0], 2);
        let offer = read_record(&mut socket, &mut cipher);
        assert_eq!(offer[0], 0x1c);
        let audio_key: [u8; 46] = offer[0x52..0x80].try_into().unwrap();
        let audio_length = usize::from(u16::from_be_bytes([offer[10], offer[11]]));
        let video_offset = 0x80 + audio_length;
        let media_key: [u8; 46] = offer[video_offset + 46..video_offset + 92]
            .try_into()
            .unwrap();
        socket
            .write_all(&cipher.encrypt(&media_answer()).unwrap())
            .unwrap();
        let payloads = media_payloads();
        let mut encryption = SrtpSender::new(&media_key, 0x1234);
        for (index, payload) in payloads.iter().enumerate() {
            let wire = encryption
                .packet(payload, 123456, 100, index + 1 == payloads.len())
                .unwrap();
            video_socket.send_to(&wire, (peer.ip(), port + 1)).unwrap();
        }
        let mut audio_sender = SrtpSender::new(&audio_key, 0x5678);
        let mut audio = &include_bytes!("../../tests/data/apple-aac-eld-stereo.aaceld")[..];
        let mut timestamp = 0u32;
        while !audio.is_empty() {
            let size = usize::from(u16::from_be_bytes([audio[0], audio[1]]));
            let wire = audio_sender
                .packet(&audio[2..size + 2], timestamp, 101, false)
                .unwrap();
            control_socket.send_to(&wire, (peer.ip(), port)).unwrap();
            audio = &audio[size + 2..];
            timestamp += 900;
        }
        let mut pointer = false;
        let mut key = false;
        let mut wheel = Vec::new();
        while !(pointer && key && wheel.len() == 10) {
            let message = read_record(&mut socket, &mut cipher);
            match message.first() {
                Some(0x10) => {
                    assert_eq!(message.len(), 18);
                    let mut block: [u8; 16] = message[2..].try_into().unwrap();
                    aes::Aes128::new((&[21; 16]).into()).decrypt_block((&mut block).into());
                    assert_eq!(block[10], 0xff);
                    assert_eq!(&block[12..], &[0, 32, 0, 24]);
                    if pointer {
                        wheel.push(block[11]);
                    } else {
                        assert_eq!(block[11], 2);
                        pointer = true;
                    }
                }
                Some(4) => {
                    assert_eq!(message, [4, 0, 0, 0, 0, 0, 0xff, 0xeb]);
                    key = true;
                }
                _ => (),
            }
        }
        assert_eq!(wheel, [10, 2, 10, 2, 10, 2, 34, 2, 34, 2]);
        observed.send(()).unwrap();
        let mut tail = [0];
        let _ = socket.read(&mut tail);
    });
    let (events, receiver) = mpsc::channel();
    let frames = Arc::new(Mutex::new(None));
    let session = Session::start(
        Request {
            profile: Profile {
                id: Uuid::new_v4(),
                name: "HEVC test".into(),
                host: "127.0.0.2".into(),
                port,
                authentication: AuthMode::MacLogin,
                has_saved_credentials: false,
            },
            credentials: CredentialSource::Manual(Credentials::new(
                "fixture".into(),
                "FixtureOnly".into(),
            )),
            streaming: StreamSettings {
                encoding: Encoding::Hevc30,
                ..Default::default()
            },
            audio: crate::audio::Settings {
                muted: true,
                ..Default::default()
            },
            remember: false,
            window: 0,
            generation: 1,
            audit: audit.audit().scope(json!({"test.fixture":"hevc"})),
            clipboard: true,
        },
        events,
        frames.clone(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
            .1
        {
            Event::Connected {
                server_name,
                credentials_saved,
                ..
            } => {
                assert_eq!(server_name, "HEVC \u{2019} fixture");
                assert!(!credentials_saved);
                break;
            }
            Event::Error(error) => panic!("HEVC fixture failed: {error}"),
            _ => (),
        }
    }
    let frame = loop {
        if let Some(frame) = frames.lock().unwrap().clone() {
            break frame;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!((frame.size.width, frame.size.height), (64, 48));
    assert_eq!(&frame.rgba[..4], &[0, 42, 0, 255]);
    assert_eq!(
        session
            .counters
            .encoding
            .load(std::sync::atomic::Ordering::Relaxed),
        super::COUNTER_ENCODING
    );
    assert!(session.share_clipboard(true).is_err());
    assert!(
        session
            .send(Command::Configure(StreamSettings {
                encoding: Encoding::Auto,
                ..Default::default()
            }))
            .is_err()
    );
    session
        .send(Command::Pointer {
            x: 32,
            y: 24,
            buttons: 4,
        })
        .unwrap();
    session
        .send(Command::Scroll {
            x: 32,
            y: 24,
            buttons: 4,
            steps: crate::input::ScrollSteps::new(2, 3).unwrap(),
        })
        .unwrap();
    session
        .send(Command::Key {
            keysym: 0xffeb,
            down: false,
        })
        .unwrap();
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    let audio_deadline = Instant::now() + Duration::from_secs(5);
    while !std::fs::read_to_string(audit.audit().path())
        .unwrap()
        .contains("media.audio.first_frame")
    {
        assert!(
            Instant::now() < audio_deadline,
            "Encrypted native Apple audio did not decode"
        );
        thread::sleep(Duration::from_millis(5));
    }
    session.finish().unwrap();
    server.join().unwrap();
    audit.shutdown().unwrap();
    let log = std::fs::read_to_string(audit.audit().path()).unwrap();
    assert!(log.contains("media.hevc.first_frame"));
    assert!(log.contains("media.audio.first_frame"));
    assert!(log.contains("media.audio.codec.process.closed"));
    assert!(!log.contains("FixtureOnly"));
    crate::audit::verify(&[audit.audit().path()]).unwrap();
}
