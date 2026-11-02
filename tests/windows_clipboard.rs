use maccanvas::clipboard::native;
use windows::{
    Win32::{
        System::{
            StationsAndDesktops::{
                CloseDesktop, CloseWindowStation, CreateDesktopW, CreateWindowStationW,
                DESKTOP_CONTROL_FLAGS, GetProcessWindowStation, GetThreadDesktop,
                SetProcessWindowStation, SetThreadDesktop,
            },
            Threading::GetCurrentThreadId,
        },
        UI::WindowsAndMessaging::{
            CWF_CREATE_ONLY, CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WINSTA_ALL_ACCESS,
            WS_POPUP,
        },
    },
    core::{HSTRING, w},
};

#[test]
#[ignore = "Requires permission to create a private window station; CI runs this explicitly without touching an interactive user's clipboard"]
fn native_unicode_and_echo_prevention_use_a_private_station_not_the_users_clipboard() {
    unsafe {
        let previous_station = GetProcessWindowStation().unwrap();
        let previous_desktop = GetThreadDesktop(GetCurrentThreadId()).unwrap();
        // CI creates a fresh named station with naming permission; never reuse an
        // existing station or fall back to the interactive user's clipboard.
        let name = HSTRING::from(format!(
            "MacCanvas.ClipboardFixture.{}",
            uuid::Uuid::new_v4().simple()
        ));
        let station =
            CreateWindowStationW(&name, CWF_CREATE_ONLY, WINSTA_ALL_ACCESS as u32, None).unwrap();
        SetProcessWindowStation(station).unwrap();
        let desktop = CreateDesktopW(
            w!("ClipboardFixture"),
            None,
            None,
            DESKTOP_CONTROL_FLAGS(0),
            0x01ff,
            None,
        )
        .unwrap();
        SetThreadDesktop(desktop).unwrap();
        let window = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("MacCanvas clipboard fixture"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let owner = window.0 as isize;
        let result = std::panic::catch_unwind(|| {
            let text = "Native caf\u{e9} \u{1f34e}\nsecond line";
            let mut watcher = native::Watcher::default();
            watcher.receive(owner, text).unwrap();
            assert_eq!(&*native::read(owner).unwrap().unwrap(), text);
            assert!(
                watcher.paste(owner).unwrap().is_none(),
                "remote clipboard writes must not echo"
            );
            native::write(owner, "new Windows text").unwrap();
            let (sequence, text) = watcher.paste(owner).unwrap().unwrap();
            assert_eq!(&*text, "new Windows text");
            watcher.sent(sequence);
            assert!(watcher.paste(owner).unwrap().is_none());
            native::write(owner, "").unwrap();
            assert!(native::read(owner).unwrap().unwrap().is_empty());
            assert!(native::write(owner, "bad\0text").is_err());
            let large = "\n".repeat(1024 * 1024 - 1);
            native::write(owner, &large).unwrap();
            assert_eq!(&*native::read(owner).unwrap().unwrap(), &large);
        });
        DestroyWindow(window).unwrap();
        SetProcessWindowStation(previous_station).unwrap();
        SetThreadDesktop(previous_desktop).unwrap();
        CloseDesktop(desktop).unwrap();
        CloseWindowStation(station).unwrap();
        result.unwrap();
    }
}
