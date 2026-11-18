#![allow(clippy::disallowed_macros)]
//! Measurements for the DXGI presenter. Not part of the change; run with
//! `cargo test --lib probe -- --ignored --nocapture --test-threads=1`.

use super::*;
use crate::backends::win32::dxgi;
use std::sync::atomic::AtomicBool;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId};

static OS_PRINTED: AtomicBool = AtomicBool::new(false);

fn os() {
    if !OS_PRINTED.swap(true, Ordering::Relaxed) {
        let out = std::process::Command::new("cmd")
            .args(["/c", "ver"])
            .output()
            .unwrap();
        println!("OS: {}", String::from_utf8_lossy(&out.stdout).trim());
    }
}

struct ProbeWindow(HWND, u32, u32);

impl ProbeWindow {
    fn new(width: u32, height: u32, ex_style: u32, parent: HWND, child: bool) -> Self {
        let _ = Window::new(); // registers the class and sets DPI awareness
        let class = windows_sys::w!("softbuffer test window");
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        let style = if child {
            wm::WS_CHILD | wm::WS_VISIBLE
        } else {
            wm::WS_OVERLAPPEDWINDOW | wm::WS_VISIBLE
        };
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        if !child {
            unsafe { wm::AdjustWindowRectEx(&mut frame, style, 0, ex_style) };
        }
        let (x, y) = if child { (40, 30) } else { (60, 60) };
        let hwnd = unsafe {
            wm::CreateWindowExW(
                ex_style,
                class,
                windows_sys::w!("probe"),
                style,
                x,
                y,
                frame.right - frame.left,
                frame.bottom - frame.top,
                parent,
                ptr::null_mut(),
                instance,
                ptr::null(),
            )
        };
        assert!(!hwnd.is_null());
        pump();
        Self(hwnd, width, height)
    }

    fn capture(&self) -> Vec<u32> {
        capture_hwnd(self.0, self.1, self.2)
    }
}

impl Drop for ProbeWindow {
    fn drop(&mut self) {
        unsafe { wm::DestroyWindow(self.0) };
    }
}

unsafe impl Send for ProbeWindow {}
unsafe impl Sync for ProbeWindow {}

impl HasWindowHandle for ProbeWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let handle = Win32WindowHandle::new(NonZeroIsize::new(self.0 as isize).unwrap());
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(handle)) })
    }
}

impl HasDisplayHandle for ProbeWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::windows())
    }
}

fn capture_hwnd(hwnd: HWND, width: u32, height: u32) -> Vec<u32> {
    unsafe {
        let dc = Gdi::CreateCompatibleDC(ptr::null_mut());
        let mut info: Gdi::BITMAPINFO = mem::zeroed();
        info.bmiHeader = Gdi::BITMAPINFOHEADER {
            biSize: mem::size_of::<Gdi::BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: Gdi::BI_RGB,
            ..mem::zeroed()
        };
        let mut bits = ptr::null_mut();
        let bitmap = Gdi::CreateDIBSection(
            dc,
            &info,
            Gdi::DIB_RGB_COLORS,
            &mut bits,
            ptr::null_mut(),
            0,
        );
        let previous = Gdi::SelectObject(dc, bitmap);
        let printed = PrintWindow(hwnd, dc, PW_CLIENTONLY | PW_RENDERFULLCONTENT);
        let pixels = slice::from_raw_parts(bits.cast::<u32>(), (width * height) as usize)
            .iter()
            .map(|pixel| pixel & 0xffffff)
            .collect();
        Gdi::SelectObject(dc, previous);
        Gdi::DeleteObject(bitmap);
        Gdi::DeleteDC(dc);
        if printed == 0 {
            println!("  PrintWindow failed");
        }
        pixels
    }
}

/// Fraction of pixels (inside an 8 px margin) that equal `color`, after pumping for a while.
fn share(window: &ProbeWindow, color: u32, wait_ms: u64) -> f64 {
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    let mut best = 0.0;
    loop {
        pump();
        let pixels = window.capture();
        let (mut hit, mut total) = (0, 0);
        for y in 8..window.2 - 8 {
            for x in 8..window.1 - 8 {
                total += 1;
                if pixels[(y * window.1 + x) as usize] == color {
                    hit += 1;
                }
            }
        }
        let s = hit as f64 / total as f64;
        if s > best {
            best = s;
        }
        if best >= 1.0 || Instant::now() > deadline {
            return best;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn sample(window: &ProbeWindow, x: u32, y: u32) -> u32 {
    pump();
    window.capture()[(y * window.1 + x) as usize]
}

fn fill(buffer: &mut crate::Buffer<'_>, color: u32) {
    let [b, g, r, _] = color.to_le_bytes();
    buffer.pixels().fill(Pixel::new_rgb(r, g, b));
}

fn kind<D, W>(surface: &mut Surface<D, W>) -> &'static str {
    match presenter(surface) {
        Presenter::Dxgi(_) => "DXGI",
        Presenter::Gdi(_) => "GDI",
    }
}

fn hr(code: i32) -> String {
    format!("{:#010x}", code as u32)
}

fn memory() -> (f64, f64, usize) {
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS_EX = mem::zeroed();
        counters.cb = mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            counters.cb,
        );
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        let mut entry: THREADENTRY32 = mem::zeroed();
        entry.dwSize = mem::size_of::<THREADENTRY32>() as u32;
        let pid = GetCurrentProcessId();
        let mut threads = 0;
        if Thread32First(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32OwnerProcessID == pid {
                    threads += 1;
                }
                if Thread32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        windows_sys::Win32::Foundation::CloseHandle(snapshot);
        (
            counters.PrivateUsage as f64 / 1048576.0,
            counters.WorkingSetSize as f64 / 1048576.0,
            threads,
        )
    }
}

#[test]
#[ignore]
fn probe_gdi_after_flip_present() {
    os();
    println!("== GDI after a flip model present");
    let window = ProbeWindow::new(WIDTH, HEIGHT, 0, ptr::null_mut(), false);
    let context = Context::new(&window).unwrap();
    {
        let mut surface = Surface::new(&context, &window).unwrap();
        println!("  first surface: {}", kind(&mut surface));
        surface
            .resize(
                NonZeroU32::new(WIDTH).unwrap(),
                NonZeroU32::new(HEIGHT).unwrap(),
            )
            .unwrap();
        let mut buffer = surface.next_buffer().unwrap();
        fill(&mut buffer, RED);
        buffer.present().unwrap();
        println!("  red shown: {:.3}", share(&window, RED, 2000));
    }
    thread::sleep(Duration::from_millis(300));
    println!(
        "  after dropping the surface, centre pixel: {:06x}",
        sample(&window, WIDTH / 2, HEIGHT / 2)
    );
    let mut surface = Surface::new(&context, &window).unwrap();
    *presenter(&mut surface) = Presenter::Gdi(GdiPresenter::new(window.0).unwrap());
    surface
        .resize(
            NonZeroU32::new(WIDTH).unwrap(),
            NonZeroU32::new(HEIGHT).unwrap(),
        )
        .unwrap();
    let mut buffer = surface.next_buffer().unwrap();
    fill(&mut buffer, GREEN);
    buffer.present().unwrap();
    println!(
        "  GDI green shown after flip present: {:.3}",
        share(&window, GREEN, 3000)
    );
    unsafe { Gdi::InvalidateRect(window.0, ptr::null(), 1) };
    let mut buffer = surface.next_buffer().unwrap();
    fill(&mut buffer, GREEN);
    buffer.present().unwrap();
    println!(
        "  ...after InvalidateRect + present: {:.3}",
        share(&window, GREEN, 2000)
    );
    // A new DXGI surface on the same window still works?
    drop(surface);
    let mut surface = Surface::new(&context, &window).unwrap();
    surface
        .resize(
            NonZeroU32::new(WIDTH).unwrap(),
            NonZeroU32::new(HEIGHT).unwrap(),
        )
        .unwrap();
    let mut buffer = surface.next_buffer().unwrap();
    fill(&mut buffer, BLUE);
    buffer.present().unwrap();
    println!(
        "  new {} surface blue: {:.3}",
        kind(&mut surface),
        share(&window, BLUE, 2000)
    );

    let control = ProbeWindow::new(WIDTH, HEIGHT, 0, ptr::null_mut(), false);
    let mut surface = Surface::new(&context, &control).unwrap();
    *presenter(&mut surface) = Presenter::Gdi(GdiPresenter::new(control.0).unwrap());
    surface
        .resize(
            NonZeroU32::new(WIDTH).unwrap(),
            NonZeroU32::new(HEIGHT).unwrap(),
        )
        .unwrap();
    let mut buffer = surface.next_buffer().unwrap();
    fill(&mut buffer, GREEN);
    buffer.present().unwrap();
    println!(
        "  control GDI window green: {:.3}",
        share(&control, GREEN, 3000)
    );
}

fn timing_run(width: u32, height: u32, gdi: bool) {
    let window = ProbeWindow::new(width, height, 0, ptr::null_mut(), false);
    let context = Context::new(&window).unwrap();
    let mut surface = Surface::new(&context, &window).unwrap();
    if gdi {
        *presenter(&mut surface) = Presenter::Gdi(GdiPresenter::new(window.0).unwrap());
    }
    surface
        .resize(
            NonZeroU32::new(width).unwrap(),
            NonZeroU32::new(height).unwrap(),
        )
        .unwrap();
    let colors = [RED, GREEN, BLUE, YELLOW];
    for (label, partial) in [("full", false), ("partial", true)] {
        let mut codes = std::collections::BTreeMap::new();
        let (mut map_max, mut map_total, mut present_max, mut present_total) = (
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        );
        let start = Instant::now();
        let frames = 120;
        for i in 0..frames {
            pump();
            let t = Instant::now();
            let mut buffer = surface.next_buffer().unwrap();
            let m = t.elapsed();
            fill(&mut buffer, colors[i % 4]);
            let t = Instant::now();
            if partial {
                buffer.present_with_damage(&[rect(10, 10, 64, 64)]).unwrap();
            } else {
                buffer.present().unwrap();
            }
            let p = t.elapsed();
            *codes
                .entry(hr(dxgi::LAST_PRESENT.load(Ordering::Relaxed)))
                .or_insert(0) += 1;
            map_max = map_max.max(m);
            map_total += m;
            present_max = present_max.max(p);
            present_total += p;
        }
        let total = start.elapsed();
        println!(
            "  {} {width}x{height} {label}: {frames} frames in {:.0} ms ({:.0} fps), next_buffer avg {:.2} max {:.2} ms, present avg {:.2} max {:.2} ms, codes {:?}",
            kind(&mut surface),
            total.as_secs_f64() * 1e3,
            frames as f64 / total.as_secs_f64(),
            map_total.as_secs_f64() * 1e3 / frames as f64,
            map_max.as_secs_f64() * 1e3,
            present_total.as_secs_f64() * 1e3 / frames as f64,
            present_max.as_secs_f64() * 1e3,
            if gdi { Default::default() } else { codes },
        );
    }
}

#[test]
#[ignore]
fn probe_present_timing() {
    os();
    println!("== Back-to-back presents (does presenting wait for vblank?)");
    for (w, h) in [(200, 150), (800, 600)] {
        timing_run(w, h, false);
        timing_run(w, h, true);
    }
}

#[test]
#[ignore]
fn probe_minimized_hidden_occluded() {
    os();
    println!("== Presenting while not visible");
    for state in ["minimized", "hidden", "covered"] {
        let window = Arc::new(ProbeWindow::new(WIDTH, HEIGHT, 0, ptr::null_mut(), false));
        let context = Context::new(window.clone()).unwrap();
        let mut surface = Surface::new(&context, window.clone()).unwrap();
        surface
            .resize(
                NonZeroU32::new(WIDTH).unwrap(),
                NonZeroU32::new(HEIGHT).unwrap(),
            )
            .unwrap();
        let mut buffer = surface.next_buffer().unwrap();
        fill(&mut buffer, RED);
        buffer.present().unwrap();
        let _ = share(&window, RED, 2000);
        let cover = (state == "covered").then(|| {
            let cover = ProbeWindow::new(
                WIDTH + 200,
                HEIGHT + 200,
                wm::WS_EX_TOPMOST,
                ptr::null_mut(),
                false,
            );
            unsafe {
                wm::SetWindowPos(
                    cover.0,
                    wm::HWND_TOPMOST,
                    0,
                    0,
                    WIDTH as i32 + 400,
                    HEIGHT as i32 + 400,
                    wm::SWP_SHOWWINDOW,
                )
            };
            cover
        });
        match state {
            "minimized" => unsafe {
                wm::ShowWindow(window.0, wm::SW_MINIMIZE);
            },
            "hidden" => unsafe {
                wm::ShowWindow(window.0, wm::SW_HIDE);
            },
            _ => {}
        }
        pump();
        thread::sleep(Duration::from_millis(200));
        for ignore_occlusion in [false, true] {
            dxgi::IGNORE_OCCLUSION.store(ignore_occlusion, Ordering::Relaxed);
            let (done, finished) = mpsc::channel();
            thread::spawn(move || {
                let mut codes = std::collections::BTreeMap::new();
                let start = Instant::now();
                let mut max = Duration::ZERO;
                for i in 0..60u32 {
                    let t = Instant::now();
                    let mut buffer = surface.next_buffer().unwrap();
                    let damage = rect(10 + (i % 6) * 25, 20, 20, 20);
                    for y in 20..40 {
                        for x in damage.x..damage.x + 20 {
                            let stride = buffer.byte_stride().get() / 4;
                            buffer.pixels()[(y * stride + x) as usize] = if i % 2 == 0 {
                                Pixel::new_rgb(0, 0, 255)
                            } else {
                                Pixel::new_rgb(0, 255, 0)
                            };
                        }
                    }
                    buffer.present_with_damage(&[damage]).unwrap();
                    max = max.max(t.elapsed());
                    *codes
                        .entry(hr(dxgi::LAST_PRESENT.load(Ordering::Relaxed)))
                        .or_insert(0) += 1;
                }
                done.send((surface, start.elapsed(), max, codes)).unwrap();
            });
            let deadline = Instant::now() + Duration::from_secs(20);
            let result = loop {
                pump();
                match finished.recv_timeout(Duration::from_millis(10)) {
                    Ok(r) => break Some(r),
                    Err(_) if Instant::now() < deadline => {}
                    Err(_) => break None,
                }
            };
            let Some((s, total, max, codes)) = result else {
                println!("  {state} (ignore occlusion {ignore_occlusion}): 60 presents BLOCKED for >20 s");
                return;
            };
            surface = s;
            println!(
                "  {state} (ignore occlusion {ignore_occlusion}): 60 partial presents in {:.0} ms, max {:.1} ms, codes {codes:?}",
                total.as_secs_f64() * 1e3,
                max.as_secs_f64() * 1e3
            );
        }
        dxgi::IGNORE_OCCLUSION.store(true, Ordering::Relaxed);
        drop(cover);
        match state {
            "minimized" => unsafe {
                wm::ShowWindow(window.0, wm::SW_RESTORE);
            },
            "hidden" => unsafe {
                wm::ShowWindow(window.0, wm::SW_SHOW);
            },
            _ => {}
        }
        thread::sleep(Duration::from_millis(500));
        pump();
        // The last frame: red with alternating squares, the last (i=59) being green at slot 5.
        let shown = window.capture();
        let at = |x: u32, y: u32| shown[(y * WIDTH + x) as usize];
        println!(
            "  {state}: after showing again without presenting: background {:06x}, squares {:06x} {:06x} {:06x} {:06x} {:06x} {:06x}",
            at(100, 100), at(20, 30), at(45, 30), at(70, 30), at(95, 30), at(120, 30), at(145, 30)
        );
        // Now a partial present elsewhere (ignoring occlusion, so no whole frame).
        let mut buffer = surface.next_buffer().unwrap();
        for y in 100..120u32 {
            for x in 20..40u32 {
                let stride = buffer.byte_stride().get() / 4;
                buffer.pixels()[(y * stride + x) as usize] = Pixel::new_rgb(255, 255, 0);
            }
        }
        buffer
            .present_with_damage(&[rect(20, 100, 20, 20)])
            .unwrap();
        thread::sleep(Duration::from_millis(300));
        pump();
        let shown = window.capture();
        let at = |x: u32, y: u32| shown[(y * WIDTH + x) as usize];
        println!(
            "  {state}: after one more partial present: yellow {:06x}, background {:06x}, squares {:06x} {:06x} {:06x} {:06x} {:06x} {:06x}",
            at(30, 110), at(100, 100), at(20, 30), at(45, 30), at(70, 30), at(95, 30), at(120, 30), at(145, 30)
        );
        dxgi::IGNORE_OCCLUSION.store(false, Ordering::Relaxed);
    }
}

#[test]
#[ignore]
fn probe_device_cost() {
    os();
    println!("== Cost of a surface (800x600)");
    let base = memory();
    println!(
        "  baseline: {:.1} MiB private, {:.1} MiB working set, {} threads",
        base.0, base.1, base.2
    );
    let mut keep = Vec::new();
    for gdi in [true, false] {
        for n in 1..=3 {
            let window: &'static ProbeWindow = Box::leak(Box::new(ProbeWindow::new(
                800,
                600,
                0,
                ptr::null_mut(),
                false,
            )));
            let t = Instant::now();
            let context = Context::new(&*window).unwrap();
            let mut surface = Surface::new(&context, &*window).unwrap();
            let created = t.elapsed();
            if gdi {
                *presenter(&mut surface) = Presenter::Gdi(GdiPresenter::new(window.0).unwrap());
            }
            let t = Instant::now();
            surface
                .resize(NonZeroU32::new(800).unwrap(), NonZeroU32::new(600).unwrap())
                .unwrap();
            let configured = t.elapsed();
            let t = Instant::now();
            let mut buffer = surface.next_buffer().unwrap();
            fill(&mut buffer, RED);
            buffer.present().unwrap();
            let presented = t.elapsed();
            let _ = share(window, RED, 1000);
            thread::sleep(Duration::from_millis(300));
            let m = memory();
            println!(
                "  {} surface #{n}: new {:.1} ms, configure {:.1} ms, first frame {:.1} ms; now {:.1} MiB private (+{:.1}), {:.1} MiB working set (+{:.1}), {} threads (+{})",
                kind(&mut surface),
                created.as_secs_f64() * 1e3,
                configured.as_secs_f64() * 1e3,
                presented.as_secs_f64() * 1e3,
                m.0, m.0 - base.0, m.1, m.1 - base.1, m.2, m.2 as i64 - base.2 as i64
            );
            keep.push((context, surface));
        }
    }
}

#[test]
#[ignore]
fn probe_window_kinds() {
    os();
    println!("== Window kinds");
    for (label, ex_style) in [
        ("layered (alpha 255)", wm::WS_EX_LAYERED),
        ("no redirection bitmap", wm::WS_EX_NOREDIRECTIONBITMAP),
        (
            "layered + transparent (winit cursor hittest off)",
            wm::WS_EX_LAYERED | wm::WS_EX_TRANSPARENT,
        ),
    ] {
        let window = ProbeWindow::new(WIDTH, HEIGHT, ex_style, ptr::null_mut(), false);
        if ex_style & wm::WS_EX_LAYERED != 0 {
            unsafe { wm::SetLayeredWindowAttributes(window.0, 0, 255, wm::LWA_ALPHA) };
        }
        let context = Context::new(&window).unwrap();
        let mut surface = Surface::new(&context, &window).unwrap();
        let configured = surface.resize(
            NonZeroU32::new(WIDTH).unwrap(),
            NonZeroU32::new(HEIGHT).unwrap(),
        );
        let k = kind(&mut surface);
        let mut buffer = surface.next_buffer().unwrap();
        fill(&mut buffer, RED);
        let presented = buffer.present();
        println!(
            "  {label}: configure {:?}, now {k}, present {:?}, red shown {:.3}",
            configured.map_err(|e| e.to_string()),
            presented.map_err(|e| e.to_string()),
            share(&window, RED, 2000)
        );
    }

    // Becomes layered after the first DXGI present.
    let window = ProbeWindow::new(WIDTH, HEIGHT, 0, ptr::null_mut(), false);
    let context = Context::new(&window).unwrap();
    let mut surface = Surface::new(&context, &window).unwrap();
    surface
        .resize(
            NonZeroU32::new(WIDTH).unwrap(),
            NonZeroU32::new(HEIGHT).unwrap(),
        )
        .unwrap();
    let mut buffer = surface.next_buffer().unwrap();
    fill(&mut buffer, RED);
    buffer.present().unwrap();
    let before = share(&window, RED, 2000);
    unsafe {
        let style = wm::GetWindowLongW(window.0, wm::GWL_EXSTYLE) as u32;
        wm::SetWindowLongW(
            window.0,
            wm::GWL_EXSTYLE,
            (style | wm::WS_EX_LAYERED | wm::WS_EX_TRANSPARENT) as i32,
        );
        wm::SetLayeredWindowAttributes(window.0, 0, 255, wm::LWA_ALPHA);
    }
    let mut buffer = surface.next_buffer().unwrap();
    fill(&mut buffer, GREEN);
    let presented = buffer.present();
    println!(
        "  made layered after presenting: red before {before:.3}, present {:?}, green after {:.3}",
        presented.map_err(|e| e.to_string()),
        share(&window, GREEN, 2000)
    );

    // Child window.
    let parent = ProbeWindow::new(300, 220, 0, ptr::null_mut(), false);
    let child = ProbeWindow::new(WIDTH, HEIGHT, 0, parent.0, true);
    let context = Context::new(&child).unwrap();
    let mut surface = Surface::new(&context, &child).unwrap();
    surface
        .resize(
            NonZeroU32::new(WIDTH).unwrap(),
            NonZeroU32::new(HEIGHT).unwrap(),
        )
        .unwrap();
    let k = kind(&mut surface);
    let mut buffer = surface.next_buffer().unwrap();
    fill(&mut buffer, BLUE);
    let presented = buffer.present();
    thread::sleep(Duration::from_millis(300));
    pump();
    let shown = parent.capture();
    let at = |x: u32, y: u32| shown[(y * 300 + x) as usize];
    println!(
        "  child window ({k}): present {:?}, child via PrintWindow blue {:.3}, parent capture inside child {:06x}, outside {:06x}",
        presented.map_err(|e| e.to_string()),
        share(&child, BLUE, 2000),
        at(40 + 100, 30 + 75),
        at(10, 10)
    );

    // Two surfaces on one window.
    let window = ProbeWindow::new(WIDTH, HEIGHT, 0, ptr::null_mut(), false);
    let context = Context::new(&window).unwrap();
    let mut first = Surface::new(&context, &window).unwrap();
    first
        .resize(
            NonZeroU32::new(WIDTH).unwrap(),
            NonZeroU32::new(HEIGHT).unwrap(),
        )
        .unwrap();
    let mut second = Surface::new(&context, &window).unwrap();
    let configured = second.resize(
        NonZeroU32::new(WIDTH).unwrap(),
        NonZeroU32::new(HEIGHT).unwrap(),
    );
    println!(
        "  second surface on one window: configure {:?}, now {}",
        configured.map_err(|e| e.to_string()),
        kind(&mut second)
    );

    // Frame smaller than the window: what fills the rest?
    let window = ProbeWindow::new(WIDTH, HEIGHT, 0, ptr::null_mut(), false);
    let context = Context::new(&window).unwrap();
    let mut surface = Surface::new(&context, &window).unwrap();
    surface
        .resize(
            NonZeroU32::new(WIDTH / 2).unwrap(),
            NonZeroU32::new(HEIGHT / 2).unwrap(),
        )
        .unwrap();
    let mut buffer = surface.next_buffer().unwrap();
    fill(&mut buffer, RED);
    buffer.present().unwrap();
    thread::sleep(Duration::from_millis(300));
    println!(
        "  smaller frame: inside {:06x}, outside {:06x}",
        sample(&window, 20, 20),
        sample(&window, WIDTH - 20, HEIGHT - 20)
    );
}
