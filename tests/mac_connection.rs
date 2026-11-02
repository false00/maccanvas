use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::Duration,
};

use aes::cipher::{BlockDecrypt, KeyInit};
use crypto_bigint::{
    BoxedUint,
    modular::{BoxedMontyForm, BoxedMontyParams},
};
use md5::{Digest, Md5};

use maccanvas::{
    auth::Credentials,
    geometry::{Extent, Viewport},
    input::Command,
    profiles::AuthMode,
    protocol::{Decoder, Message, PIXEL_FORMAT, handshake},
};

const PRIME: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1",
    "29024E088A67CC74020BBEA63B139B22514A08798E3404DD",
    "EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245",
    "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE65381",
    "FFFFFFFFFFFFFFFF"
);

fn read<const N: usize>(stream: &mut TcpStream) -> [u8; N] {
    let mut buffer = [0; N];
    stream.read_exact(&mut buffer).unwrap();
    buffer
}

fn server_authenticate(stream: &mut TcpStream) {
    stream.write_all(b"RFB 003.889\n").unwrap();
    assert_eq!(&read::<12>(stream), b"RFB 003.008\n");
    stream.write_all(&[2, 2, 30]).unwrap();
    assert_eq!(read::<1>(stream), [30]);

    let modulus = BoxedUint::from_be_hex(PRIME, 1024).into_option().unwrap();
    let parameters =
        BoxedMontyParams::new_vartime(modulus.clone().into_odd().into_option().unwrap());
    let generator = BoxedUint::from_be_slice(&[2], 1024).unwrap();
    let exponent = BoxedUint::from_be_slice(&[0x41; 32], 1024).unwrap();
    let public = BoxedMontyForm::new(generator, &parameters)
        .pow(&exponent)
        .retrieve();
    stream.write_all(&[0, 2, 0, 128]).unwrap();
    stream.write_all(&modulus.to_be_bytes()).unwrap();
    stream.write_all(&public.to_be_bytes()).unwrap();
    let mut encrypted = read::<128>(stream);
    let peer = BoxedUint::from_be_slice(&read::<128>(stream), 1024).unwrap();
    let shared = BoxedMontyForm::new(peer, &parameters)
        .pow(&exponent)
        .retrieve();
    let key = Md5::digest(shared.to_be_bytes());
    let cipher = aes::Aes128::new_from_slice(&key).unwrap();
    for block in encrypted.as_chunks_mut::<16>().0 {
        cipher.decrypt_block(block.into());
    }
    assert_eq!(
        encrypted[..64].split(|&byte| byte == 0).next().unwrap(),
        b"fixture-user"
    );
    assert_eq!(
        encrypted[64..].split(|&byte| byte == 0).next().unwrap(),
        b"fixture-password"
    );
    stream.write_all(&[0, 0, 0, 0]).unwrap();
    assert_eq!(read::<1>(stream), [1]);
}

#[test]
fn mac_login_four_k_frame_and_dpi_scaled_input_round_trip() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (observed, received) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        server_authenticate(&mut stream);
        stream.write_all(&3840u16.to_be_bytes()).unwrap();
        stream.write_all(&2160u16.to_be_bytes()).unwrap();
        stream.write_all(&PIXEL_FORMAT).unwrap();
        stream.write_all(&7u32.to_be_bytes()).unwrap();
        stream.write_all(b"Fixture").unwrap();
        assert_eq!(read::<4>(&mut stream), [0, 0, 0, 0]);
        assert_eq!(read::<16>(&mut stream), PIXEL_FORMAT);
        let header = read::<4>(&mut stream);
        assert_eq!(&header[..2], &[2, 0]);
        let mut encodings = vec![0; usize::from(u16::from_be_bytes([header[2], header[3]])) * 4];
        stream.read_exact(&mut encodings).unwrap();
        assert_eq!(read::<10>(&mut stream)[0], 3);

        stream
            .write_all(&[0, 0, 0, 1, 0, 0, 0, 0, 15, 0, 8, 112, 0, 0, 0, 0])
            .unwrap();
        let row: Vec<_> = (0..3840)
            .flat_map(|x| {
                if x < 1920 {
                    [0, 0, 255, 0]
                } else {
                    [0, 255, 0, 0]
                }
            })
            .collect();
        for _ in 0..2160 {
            stream.write_all(&row).unwrap();
        }
        let movement = read::<6>(&mut stream);
        let click = read::<6>(&mut stream);
        let release = read::<6>(&mut stream);
        observed.send([movement, click, release]).unwrap();
    });

    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let info = handshake(
        &mut client,
        AuthMode::MacLogin,
        &Credentials::new("fixture-user".into(), "fixture-password".into()),
    )
    .unwrap();
    assert_eq!(info.size, Extent::new(3840, 2160).unwrap());
    let mut decoder = Decoder::new(info.size);
    assert!(matches!(
        decoder.read_message(&mut client).unwrap(),
        Message::Update { changed: true }
    ));
    assert_eq!(&decoder.frame.rgba[..4], &[255, 0, 0, 255]);
    assert_eq!(&decoder.frame.rgba[3839 * 4..3840 * 4], &[0, 255, 0, 255]);
    let viewport = Viewport::fit(info.size, 1706.6666, 960.0, 1.5).unwrap();
    let center = viewport.to_remote(853.0, 480.0, false).unwrap();
    Command::Pointer {
        x: center.0,
        y: center.1,
        buttons: 0,
    }
    .write(&mut client)
    .unwrap();
    let edge = viewport.to_remote(1706.0, 959.3333, false).unwrap();
    assert_eq!(edge, (3839, 2159));
    Command::Pointer {
        x: edge.0,
        y: edge.1,
        buttons: 1,
    }
    .write(&mut client)
    .unwrap();
    Command::Pointer {
        x: edge.0,
        y: edge.1,
        buttons: 0,
    }
    .write(&mut client)
    .unwrap();
    let packets = received.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(packets[1], [5, 1, 0x0e, 0xff, 0x08, 0x6f]);
    assert_eq!(packets[2], [5, 0, 0x0e, 0xff, 0x08, 0x6f]);
    server.join().unwrap();
}

#[test]
fn mac_login_never_downgrades_to_unauthenticated_or_password_only() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(b"RFB 003.008\n").unwrap();
        read::<12>(&mut stream);
        stream.write_all(&[2, 1, 2]).unwrap();
        let mut extra = [0];
        assert_eq!(stream.read(&mut extra).unwrap(), 0);
    });
    let mut stream = TcpStream::connect(address).unwrap();
    assert!(
        handshake(
            &mut stream,
            AuthMode::MacLogin,
            &Credentials::new("fixture-user".into(), "fixture-password".into())
        )
        .is_err()
    );
    drop(stream);
    server.join().unwrap();
}
