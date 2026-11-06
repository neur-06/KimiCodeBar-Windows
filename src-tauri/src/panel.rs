#[cfg(windows)]
use std::sync::atomic::AtomicIsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, Position, Size, WebviewWindow};

/// 面板边缘与托盘图标 / 屏幕边缘之间保留的间距（物理像素）。
const EDGE_MARGIN: i32 = 8;

/// 自绘外框（阴影）每边逻辑 px。窗口用 `SetWindowRgn` 裁剪成自定义形状后，DWM 不再为
/// 窗口绘制阴影/系统边框——原先那圈"外框"会消失。这里让窗口在面板四周各留出这一圈：
/// 窗口宽 +2×pad、region 高 +2×pad、`.panel` 在窗口内 bottom 留 pad，由前端
/// `box-shadow` 把阴影画回来。region 外圆角取「面板 8 + pad」保证内外同心。
/// ⚠️ 窗口必须是 `transparent: true` + `shadow: false`（tauri.conf.json）：不透明窗口会在
/// 这一圈填 WebView2 底色（深色主题下就是实心黑边），DWM 系统阴影也会在 region 裁剪后消失。
const FRAME_PAD: f64 = 8.0;

/// 极简模式的面板紧凑逻辑高度（只显示页头 + 7天/5小时额度条 + 翻页圆点 + 底栏）。
/// 内容自适应生效后的兜底：前端尚未实测时用它做极简模式的初始高。
const MINIMAL_PANEL_HEIGHT: f64 = 350.0;

/// 内容自适应的最小窗口逻辑高：防极端内容（如只剩页头 + 底栏）把窗口压得失形。
const MIN_PANEL_HEIGHT: f64 = 200.0;

/// 内容驱动的视觉高度缓动时长 / 帧间隔（开窗前的 fit 不动画，见 fit 的 animate 参数）。
/// B 方案：窗口几何固定在「配置高（屏幕内取小）」，动画只改窗口 region（SetWindowRgn）
/// 裁剪出视觉高度，WebView2 视口全程不变——wry 给子窗口的 ASYNC SetWindowPos 与 WebView2
/// 内部布局滞后都不再影响底栏。帧间隔 16ms（约 19 帧/300ms，60Hz）：SetWindowRgn 从
/// 动画线程调用会同步消息到 GUI 线程，8ms 帧（38 次/动画）把主线程压到饱和、WebView2
/// 渲染饥饿（表现为卡顿/卡死）；300ms 动画本身够长，60Hz 的 19 帧已足够平滑。
/// 300ms + 轻微过冲（back-out 2%，见 `ease_out_back`）：前段够快、过冲后留足回落时间，
/// 不会像 200ms 那样"冲过再弹回"抖成抽搐。过冲幅度/时长改动必须同步前端 CSS transition。
const RESIZE_ANIM_MS: u64 = 300;
const RESIZE_FRAME_MS: u64 = 16;
/// 当前视觉高度（逻辑像素 f64 位模式；0 = 未设置）：动画中断时作为下一段动画的起点，
/// 避免从上一目标值跳变。
static PANEL_VISUAL_H: AtomicU64 = AtomicU64::new(0);
/// 高度动画序号（单调递增）：新动画顶掉进行中的旧动画（可中断，从实时高度起步）
static RESIZE_ANIM_SEQ: AtomicU64 = AtomicU64::new(0);
/// 在播动画的目标逻辑高（f64 位模式；0 = 无在播动画），目标防抖用
static RESIZE_ANIM_TARGET: AtomicU64 = AtomicU64::new(0);
/// 目标防抖阈值（逻辑像素）：动画进行中收到与在播目标差小于此值的新目标 → 忽略
/// （不顶序号、不重启动画），吸收内容测高的亚像素级抖动
const RESIZE_TARGET_DEBOUNCE: f64 = 4.0;

/// 带轻微过冲的缓出（back-out）：与前端 CSS `cubic-bezier(0.3, 1.3, 0.6, 1)` 严格同曲线，
/// 峰值约 1.02（2% 过冲，出现在 ~68% 时间）——窗口高度先越过目标再回落，是"力量感"来源。
/// 三处参数必须一致：CSS transition、这里、以及前端 `duration 200ms`；
/// 任一处不同步都会让 region 裁剪与 CSS 高度错位（边框/阴影抖动、内容裁切）。
fn ease_out_back(t: f64) -> f64 {
    // back-out：P1=(0.3,1.25) P2=(0.6,1)；x(s) 单调，二分求 x(s)=t 再取 y(s)。
    // 24 次迭代精度 ~1e-7，每帧一次纯数学开销可忽略。
    let (x1, y1, x2, y2) = (0.3, 1.25, 0.6, 1.0);
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    let bez = |a1: f64, a2: f64, s: f64| {
        let u = 1.0 - s;
        3.0 * u * u * s * a1 + 3.0 * u * s * s * a2 + s * s * s
    };
    let (mut lo, mut hi) = (0.0, 1.0);
    let mut s = t;
    for _ in 0..24 {
        let x = bez(x1, x2, s);
        if (x - t).abs() < 1e-9 {
            break;
        }
        if x < t {
            lo = s;
        } else {
            hi = s;
        }
        s = 0.5 * (lo + hi);
    }
    bez(y1, y2, s)
}

/// 托盘图标的物理像素区域（由 tray-icon 事件的 rect 转换而来）。
#[derive(Clone, Copy)]
pub struct TrayRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl TrayRect {
    /// 将 tray-icon 事件里的 position/size（可能是物理或逻辑坐标）统一换算成物理像素。
    pub fn new(position: Position, size: Size) -> Self {
        let (x, y) = match position {
            Position::Physical(p) => (p.x, p.y),
            Position::Logical(l) => (l.x.round() as i32, l.y.round() as i32),
        };
        let (width, height) = match size {
            Size::Physical(s) => (s.width as i32, s.height as i32),
            Size::Logical(s) => (s.width.round() as i32, s.height.round() as i32),
        };
        TrayRect {
            x,
            y,
            width,
            height,
        }
    }
}

/// 显示器几何快照（物理像素）：定位/动画启动时查询一次缓存，帧内不再做系统查询。
#[derive(Clone, Copy)]
struct MonitorGeom {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// 把 tauri 显示器句柄快照成帧内复用的纯几何（物理像素）。
fn monitor_geom(monitor: &tauri::Monitor) -> MonitorGeom {
    MonitorGeom {
        x: monitor.position().x,
        y: monitor.position().y,
        w: monitor.size().width as i32,
        h: monitor.size().height as i32,
    }
}

/// 逻辑高 → 物理像素：×缩放比后四舍五入取整（喂给 set_size(Size::Physical)，
/// 消除逻辑小数经 DPI 缩放产生的亚像素颤动），保底 1px 防 0 高窗口。
fn frame_height_px(h_logical: f64, scale: f64) -> u32 {
    (h_logical * scale).round().max(1.0) as u32
}

/// 面板定位纯数学（win_w / win_h 为窗口物理像素尺寸）：底边锚定 = 托盘上方
/// （托盘在屏幕上半部、上方放不下时翻到托盘下方），水平居中，最后 clamp 进显示器
/// 范围；mon 为 None（取不到显示器）时不翻转不 clamp，保持托盘锚定原值。
fn anchored_panel_pos(
    tray: TrayRect,
    mon: Option<MonitorGeom>,
    win_w: i32,
    win_h: i32,
) -> (i32, i32) {
    let mut x = tray.x + tray.width / 2 - win_w / 2;
    let mut y = tray.y - win_h - EDGE_MARGIN;
    if let Some(m) = mon {
        // 托盘图标位于屏幕上半部（任务栏在顶部）时，改为放到图标下方
        if y < m.y + EDGE_MARGIN {
            y = tray.y + tray.height + EDGE_MARGIN;
        }
        let min_x = m.x + EDGE_MARGIN;
        let max_x = (m.x + m.w - win_w - EDGE_MARGIN).max(min_x);
        let min_y = m.y + EDGE_MARGIN;
        let max_y = (m.y + m.h - win_h - EDGE_MARGIN).max(min_y);
        x = x.clamp(min_x, max_x);
        y = y.clamp(min_y, max_y);
    }
    (x, y)
}

/// 目标防抖判定：有在播目标（位模式非 0）且新目标与其差 < RESIZE_TARGET_DEBOUNCE
/// 逻辑 px → true（忽略新目标，不顶序号、不重启动画）。
fn should_debounce_target(in_flight_bits: u64, new_target: f64) -> bool {
    in_flight_bits != 0
        && (f64::from_bits(in_flight_bits) - new_target).abs() < RESIZE_TARGET_DEBOUNCE
}

/// 动画退出时清位在播目标：仅当在播目标仍是自己（可能已被顶号的新动画改写，不能误清）。
fn clear_anim_target_if_mine(target_h: f64) {
    if RESIZE_ANIM_TARGET.load(Ordering::SeqCst) == target_h.to_bits() {
        RESIZE_ANIM_TARGET.store(0, Ordering::SeqCst);
    }
}

/// 面板窗口「尺寸 + 位置」的串行锁：原子 SetWindowPos 直接生效、绕过 Tauri 主线程消息
/// 队列的 FIFO 保护，瞬时到位与在播动画帧可能跨线程并发改同一窗口，必须互斥——
/// 临界区 = 锁内「序号检查（或顶号）+ 设置」成对完成，旧帧不可能覆盖新结果。
static PANEL_WIN_LOCK: Mutex<()> = Mutex::new(());

/// 取锁（中毒仅发生在持锁 panic，窗口句柄操作不会 panic，继续用即可）。
fn lock_panel_win() -> MutexGuard<'static, ()> {
    PANEL_WIN_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 面板窗口 HWND 缓存：`WebviewWindow::hwnd()` 本身是同步消息 getter（channel 往返），
/// 动画每帧调用会被主线程拖住；主窗口不重建，首次读到后进程内复用。
#[cfg(windows)]
static PANEL_HWND: AtomicIsize = AtomicIsize::new(0);

#[cfg(windows)]
fn panel_hwnd(window: &WebviewWindow) -> Option<*mut std::ffi::c_void> {
    let cached = PANEL_HWND.load(Ordering::Acquire);
    if cached != 0 {
        return Some(cached as *mut std::ffi::c_void);
    }
    let hwnd = window.hwnd().ok()?;
    let raw = hwnd.0 as isize;
    PANEL_HWND.store(raw, Ordering::Release);
    Some(raw as *mut std::ffi::c_void)
}

/// 窗口当前是否可见：Windows 直接走 Win32 `IsWindowVisible`（无消息往返）。
/// **不能**用 `WebviewWindow::is_visible().unwrap_or(false)`：那条是同步消息，主线程忙
/// （React 渲染/IPC）时可能返回 Err，unwrap_or(false) 会把「可见」误判成隐藏——
/// refit 直接跳过（面板高度卡死不再更新）、高度动画中途退出（region 停在半截）。
#[cfg(windows)]
fn window_visible(window: &WebviewWindow) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    panel_hwnd(window).is_some_and(|h| unsafe { IsWindowVisible(h) } != 0)
}

#[cfg(not(windows))]
fn window_visible(window: &WebviewWindow) -> bool {
    window.is_visible().unwrap_or(false)
}

/// 移除窗口 region（恢复完整矩形）：fit 失败（取不到显示器/配置）时的兜底——
/// 旧 region 可能来自上一页/上一次几何，尺寸不匹配会把面板顶部裁掉。
/// 完整矩形窗口的透明区不绘制、只接收点击，视觉上完整（代价是点击区域暂时变大）。
#[cfg(windows)]
fn clear_window_region(window: &WebviewWindow) {
    use windows_sys::Win32::Graphics::Gdi::SetWindowRgn;
    if let Some(hwnd) = panel_hwnd(window) {
        // SAFETY: hwnd 为本进程活动窗口句柄；region=NULL 表示移除裁剪区域。
        unsafe {
            SetWindowRgn(hwnd, std::ptr::null_mut(), 0);
        }
    }
}

#[cfg(not(windows))]
fn clear_window_region(_window: &WebviewWindow) {}

/// 窗口客户区与外框物理尺寸：Windows 直接走 Win32（GetClientRect / GetWindowRect），
/// 不走 Tauri 的同步消息 getter——切页时主线程正忙 React + WebView2 重排，每读一次
/// 尺寸都要等主线程处理，是「预长 / 收矮响应慢」的主要来源。
/// 非 Windows 回退 Tauri getter（平台无关兜底）。
#[cfg(windows)]
fn window_sizes(window: &WebviewWindow) -> Option<(PhysicalSize<u32>, PhysicalSize<u32>)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect};
    let hwnd = panel_hwnd(window)?;
    let mut client = RECT::default();
    let mut outer = RECT::default();
    // SAFETY: hwnd 为本进程活动窗口句柄；两个 API 只写调用方提供的 RECT。
    unsafe {
        if GetClientRect(hwnd, &mut client) == 0 || GetWindowRect(hwnd, &mut outer) == 0 {
            return None;
        }
    }
    let size = |r: RECT| {
        PhysicalSize::new(
            (r.right - r.left).max(0) as u32,
            (r.bottom - r.top).max(0) as u32,
        )
    };
    Some((size(client), size(outer)))
}

#[cfg(not(windows))]
fn window_sizes(window: &WebviewWindow) -> Option<(PhysicalSize<u32>, PhysicalSize<u32>)> {
    Some((window.inner_size().ok()?, window.outer_size().ok()?))
}

/// 窗口外框与客户区在宽/高上的差值（物理像素）：Windows 无边框 + 阴影窗口带隐藏偏移
/// （tao `undecorated_with_shadows`）。`set_size` 是 inner 语义、`set_position` 是 outer
/// 语义，二者不能混用同一套尺寸——锚定与 SetWindowPos 的 cx/cy 一律用外框尺寸，
/// inner 目标值只在算外框时加回差值（由 `fit_panel_to_screen` 读一次后传入各路径）。
fn frame_extra(inner: PhysicalSize<u32>, outer: PhysicalSize<u32>) -> (i32, i32) {
    (
        outer.width.saturating_sub(inner.width) as i32,
        outer.height.saturating_sub(inner.height) as i32,
    )
}

/// 面板窗口几何（尺寸 + 位置）统一出口（物理像素），只在 show/fit/显示器变化时调用：
/// - Windows 走单条 `SetWindowPos(hwnd, x, y, outer_w, outer_h)`，位置与尺寸原子生效，
///   消除「先 set_size（顶边锚定向下长出）再 set_position（上移做底边锚定）」两条异步
///   消息之间的错位中间态；
/// - 拿不到 HWND / 非 Windows 回退 Tauri 两步 API（语义等价，仅多一条消息）。
///
/// B 方案下 `inner_w/inner_h` 恒为「配置宽 + 配置高」（固定几何），内容高只由 region 裁剪。
/// 调用方必须先持 `PANEL_WIN_LOCK`（与 region 设置串行，旧帧不会覆盖新结果）。
fn apply_inner_size_pos(
    window: &WebviewWindow,
    x: i32,
    y: i32,
    inner_w: i32,
    inner_h: i32,
    extra: (i32, i32),
) {
    #[cfg(windows)]
    if set_window_size_pos_once(window, x, y, inner_w + extra.0, inner_h + extra.1) {
        return;
    }
    let _ = window.set_size(Size::Physical(PhysicalSize::new(
        inner_w.max(1) as u32,
        inner_h.max(1) as u32,
    )));
    let _ = window.set_position(Position::Physical(PhysicalPosition::new(x, y)));
}

/// Windows：单条 SetWindowPos 同时给定位置与外框尺寸（物理像素），成功返回 true。
/// **同步**（不用 SWP_ASYNCWINDOWPOS）：fit 紧接着要按客户区算 region，异步投递会让
/// `GetClientRect` 读到旧尺寸——窗口从矮变高时 region 被旧高 clamp，面板顶部被裁。
/// 几何未变化时短路返回（切页热路径零系统调用）。
#[cfg(windows)]
fn set_window_size_pos_once(window: &WebviewWindow, x: i32, y: i32, w_px: i32, h_px: i32) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SetWindowPos, HWND_TOP, SWP_NOACTIVATE, SWP_NOZORDER,
    };
    use windows_sys::Win32::Foundation::RECT;
    let Some(hwnd) = panel_hwnd(window) else {
        return false;
    };
    let (w_px, h_px) = (w_px.max(1), h_px.max(1));
    let mut cur = RECT::default();
    // SAFETY: hwnd 为本进程活动窗口句柄；GetWindowRect 只写调用方结构体。
    let same = unsafe { GetWindowRect(hwnd, &mut cur) } != 0
        && cur.left == x
        && cur.top == y
        && cur.right - cur.left == w_px
        && cur.bottom - cur.top == h_px;
    if same {
        return true;
    }
    // SAFETY: 同上；SWP_NOZORDER 不动 Z 序、SWP_NOACTIVATE 不抢焦点；x/y/cx/cy 均为
    // 调用方钳进屏幕范围的物理像素值。
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOP,
            x,
            y,
            w_px,
            h_px,
            SWP_NOZORDER | SWP_NOACTIVATE,
        ) != 0
    }
}

/// 左键点击托盘图标：切换主面板显隐；显示前先定位到托盘图标上方。
pub fn toggle_panel(app: &AppHandle, tray: TrayRect) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if window_visible(&window) {
        let _ = window.hide();
    } else {
        show_panel(app, tray);
    }
}

/// 重新定位并显示主面板（托盘菜单“刷新”的占位行为）。
pub fn show_panel(app: &AppHandle, tray: TrayRect) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    // 开窗前校准必须瞬时（面板一出现就是对的尺寸，不播动画）；
    // fit 返回 true 时尺寸 + 位置已原子完成，不再重复定位（少一次 getter 往返）
    if !fit_panel_to_screen(app, &window, tray, false) {
        position_panel(app, &window, tray);
        // fit 失败时 region 可能是旧的（尺寸不匹配会裁掉面板顶部）：直接移除裁剪兜底
        clear_window_region(&window);
    }
    let _ = window.show();
    let _ = window.set_focus();
}

/// 面板基准逻辑高：极简模式取紧凑高，普通模式取配置高。
fn base_panel_height(config_height: f64, minimal_mode: bool) -> f64 {
    if minimal_mode {
        MINIMAL_PANEL_HEIGHT
    } else {
        config_height
    }
}

/// 小屏兜底：面板实际可用的逻辑像素高度。
/// 显示器物理高按缩放比换算成逻辑高、减去边缘间距后，与配置高取小，避免矮屏把面板裁掉。
fn effective_panel_height(config_height: f64, monitor_height_px: u32, monitor_scale: f64) -> f64 {
    let cap = monitor_height_px as f64 / monitor_scale - (2 * EDGE_MARGIN) as f64 / monitor_scale;
    config_height.min(cap).max(1.0)
}

/// 视觉高度（逻辑像素）→ 窗口 region：面板底边贴托盘，区域保留窗口底部 h，顶部裁掉。
/// `region_h_logical` 是裁剪高度、`visual_h_logical` 是面板真实视觉高（存给下一段动画
/// 起步用）——两者分开是为了「取大」策略：动画帧传 `max(当前高, 目标高)`，region 恒
/// ≥ 面板高，CSS 过渡与 region 动画即使短暂不同步也绝不会把面板顶部裁掉（裁切=截断）；
/// 变矮时 region 先保持大、到终点再收，视觉上只是上方透明区大小变化。
/// 关键：SetWindowRgn 用**窗口坐标**（含无边框阴影窗口的非客户区偏移），必须先把客户区
/// 左上换算到窗口坐标，否则 region 整体左/上偏一个阴影宽度——内容被裁一侧、另一侧露出
/// 窗口背景（浅色主题下就是“白边”）。区域用圆角矩形（Win11 窗口圆角约 8 逻辑 px），
/// 避免矩形 region 把 DWM 圆角盖成直角。
/// 成功后系统接管 region 句柄（替换时旧 region 由系统释放），失败时手动删除。
#[cfg(windows)]
fn set_window_region(
    window: &WebviewWindow,
    region_h_logical: f64,
    visual_h_logical: f64,
    scale: f64,
) -> bool {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        ClientToScreen, CreateRoundRectRgn, DeleteObject, SetWindowRgn,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect};
    let Some(hwnd) = panel_hwnd(window) else {
        return false;
    };
    let mut client = RECT::default();
    let mut outer = RECT::default();
    let mut origin = POINT { x: 0, y: 0 };
    // SAFETY: hwnd 为本进程活动窗口句柄；三个 API 只写调用方提供的结构体。
    unsafe {
        if GetClientRect(hwnd, &mut client) == 0 || GetWindowRect(hwnd, &mut outer) == 0 {
            return false;
        }
        if ClientToScreen(hwnd, &mut origin) == 0 {
            return false;
        }
    }
    let cw = (client.right - client.left).max(1);
    let ch = (client.bottom - client.top).max(1);
    let ox = origin.x - outer.left;
    let oy = origin.y - outer.top;
    // 视觉高含上下两圈自绘外框：面板底边距窗口底 pad，顶部再留 pad 给阴影
    let h_px = (((region_h_logical + 2.0 * FRAME_PAD) * scale).round() as i32).clamp(1, ch);
    // CreateRoundRectRgn 的椭圆参数是直径（半径 ×2）。外圆角 = 面板圆角 8 + pad：
    // 与外层 `.panel` 的 8px border-radius 同心，裁剪边缘不会露出直角。
    let radius = ((8.0 + FRAME_PAD) * scale).round().max(1.0) as i32;
    // SAFETY: 新 region 坐标均为非负；SetWindowRgn 成功后窗口系统接管该 region，
    // 调用方不再删除（后续 region 替换时旧 region 由系统释放）。
    let rgn =
        unsafe { CreateRoundRectRgn(ox, oy + ch - h_px, ox + cw, oy + ch, 2 * radius, 2 * radius) };
    if rgn.is_null() {
        return false;
    }
    if unsafe { SetWindowRgn(hwnd, rgn, 1) } == 0 {
        // SAFETY: SetWindowRgn 失败时 region 未被系统接管，需调用方释放。
        unsafe { DeleteObject(rgn) };
        return false;
    }
    PANEL_VISUAL_H.store(visual_h_logical.to_bits(), Ordering::SeqCst);
    true
}

#[cfg(not(windows))]
fn set_window_region(
    _window: &WebviewWindow,
    _region_h_logical: f64,
    visual_h_logical: f64,
    _scale: f64,
) -> bool {
    PANEL_VISUAL_H.store(visual_h_logical.to_bits(), Ordering::SeqCst);
    false
}

/// 当前视觉高度（逻辑 px）：从未设置过返回 None（动画从这里起步）。
fn visual_height_logical() -> Option<f64> {
    let bits = PANEL_VISUAL_H.load(Ordering::SeqCst);
    if bits == 0 {
        None
    } else {
        Some(f64::from_bits(bits))
    }
}

/// 每次显示/内容高度变化时校准面板：窗口几何固定为「配置高（显示器可用高内取小）」+
/// 底边锚定，内容高只决定 region 裁剪出的视觉高度（瞬时或 300ms 动画）——WebView2 视口
/// 全程不变，前端 CSS height 过渡同步收缩内容/底栏。
/// 全程只用逻辑像素 + 托盘所在显示器的缩放比：此时窗口可能还停在别的显示器上，
/// `window.scale_factor()` 是错的；回读尺寸只用于算外框与客户区的固定差。
///
/// 返回值：true = 几何/region 已设置（瞬时路径）或已启动 region 动画；false = 取不到
/// 配置或显示器，调用方需 `position_panel` 仅定位兜底。
fn fit_panel_to_screen(
    app: &AppHandle,
    window: &WebviewWindow,
    tray: TrayRect,
    animate: bool,
) -> bool {
    let Some(conf) = app.config().app.windows.iter().find(|w| w.label == "main") else {
        return false;
    };
    let Ok(Some(monitor)) = app.monitor_from_point(tray.x as f64, tray.y as f64) else {
        tracing::warn!(
            "panel fit: monitor_from_point failed for tray ({}, {})",
            tray.x,
            tray.y
        );
        return false; // 取不到显示器：保持配置尺寸，不压高
    };
    let scale = monitor.scale_factor();
    // 内容自适应：前端实测内容高优先（含最小高保护），未实测才回退基准高——
    // 基准高需要极简模式开关，读设置是磁盘 + JSON 解析，切页热路径（有实测高）
    // 绝不能碰，否则每次预长/收矮都多几毫秒 IO 延迟
    let base = app
        .state::<crate::commands::AppState>()
        .panel_content_height()
        .map(|h| h.max(MIN_PANEL_HEIGHT))
        .unwrap_or_else(|| {
            let minimal_mode = kimicodebar::storage::load_settings()
                .map(|s| s.minimal_mode)
                .unwrap_or(false);
            base_panel_height(conf.height, minimal_mode)
        });
    let target_h = effective_panel_height(base, monitor.size().height, scale);
    // 客户区 + 外框尺寸一次读完（Win32 直读，零消息往返）：只用于算外框差（阴影偏移）。
    let sizes = window_sizes(window);
    let extra = match sizes {
        Some((inner, outer)) => frame_extra(inner, outer),
        None => (0, 0),
    };
    let mon = monitor_geom(&monitor);
    // B 方案固定窗口几何：窗口高度取「配置高（屏幕可用高内取小）」，再受托盘锚定方向上的
    // 可用空间限制——矮屏 / 任务栏在顶部时，否则窗口底会被 clamp 出屏、region 底部盖住托盘。
    // 内容高只决定 region 视觉高度；窗口/视口全程不变。
    // 自绘外框：窗口比面板上下各多 FRAME_PAD（窗口尺寸下面统一 +2×pad），fixed_h 先扣掉。
    let mut fixed_h = (effective_panel_height(conf.height, monitor.size().height, scale)
        - 2.0 * FRAME_PAD)
        .max(MIN_PANEL_HEIGHT);
    let top_room = (tray.y - 2 * EDGE_MARGIN - mon.y - extra.1) as f64 / scale - 2.0 * FRAME_PAD;
    let bottom_room = (mon.y + mon.h - (tray.y + tray.height) - 2 * EDGE_MARGIN - extra.1) as f64
        / scale
        - 2.0 * FRAME_PAD;
    let flips =
        tray.y - frame_height_px(fixed_h + 2.0 * FRAME_PAD, scale) as i32 - extra.1 - EDGE_MARGIN
            < mon.y + EDGE_MARGIN;
    fixed_h = fixed_h
        .min(if flips { bottom_room } else { top_room })
        .max(MIN_PANEL_HEIGHT);
    // 内容实测高不得超过窗口可承载的面板高（fixed_h 已扣掉上下外框）：超长内容由页内滚动消化
    let target_h = target_h.min(fixed_h);
    tracing::info!(
        "panel fit: base={base:.1} target={target_h:.1} fixed={fixed_h:.1} flips={flips} scale={scale:.2} mon=({},{},{}x{}) tray=({},{},{}x{})",
        mon.x,
        mon.y,
        mon.w,
        mon.h,
        tray.x,
        tray.y,
        tray.width,
        tray.height
    );
    let w_px = frame_height_px(conf.width + 2.0 * FRAME_PAD, scale) as i32;
    let fixed_h_px = frame_height_px(fixed_h + 2.0 * FRAME_PAD, scale) as i32;
    // 窗口几何（固定高 + 底边锚定，瞬时、幂等）：尺寸与内容无关，切页时重复设置同尺寸
    // 不会触发 WebView2 resize。
    let (x, y) = anchored_panel_pos(tray, Some(mon), w_px + extra.0, fixed_h_px + extra.1);
    // 自绘外框：窗口底边贴托盘图标顶（原先 EDGE_MARGIN 的间距改由 `.panel` 的 bottom pad
    // 承担），这样面板底边到托盘仍是 EDGE_MARGIN，底部 pad 区留给阴影。翻转时面板贴窗口顶，
    // 不再下移（保持原行为，翻转场景的 region 方向不在本次改动范围）。
    let y = if flips {
        y
    } else {
        y + frame_height_px(FRAME_PAD, scale) as i32
    };
    {
        let _guard = lock_panel_win();
        if !animate {
            RESIZE_ANIM_SEQ.fetch_add(1, Ordering::SeqCst);
            RESIZE_ANIM_TARGET.store(0, Ordering::SeqCst);
        }
        apply_inner_size_pos(window, x, y, w_px, fixed_h_px, extra);
        if !animate {
            // 瞬时路径：顶号与 region 设置在同一临界区，旧动画帧不可能覆盖。
            set_window_region(window, target_h, target_h, scale);
        }
    }
    if animate {
        animate_region_to(window, target_h, scale);
    }
    true
}

/// 把窗口 region 的视觉高度从当前值缓动到目标值（back-out 300ms @ 60Hz，2% 轻微过冲）。
/// 只改窗口形状（SetWindowRgn），不移动/缩放窗口：WebView2 视口固定，前端 CSS height
/// 过渡同步驱动内容与底栏，底部 dots/footer 不再受 WebView2 resize 滞后影响。
/// 新的调用顶掉进行中的动画（序号失效即退出，下一次从 PANEL_VISUAL_H 重新起步，可中断）；
/// 与在播目标差 < 4 逻辑 px 的新目标被防抖忽略（不顶号、不重启，吸收测高抖动）；
/// 面板中途被隐藏则让出（下次 show 时 fit 会一次性校准）。
fn animate_region_to(window: &WebviewWindow, target_h: f64, scale: f64) {
    let start_h = visual_height_logical().unwrap_or(target_h);
    // 起点即目标：不动画，直接设 region（顶号，与瞬时路径同语义）
    if (start_h - target_h).abs() < 2.0 {
        let _guard = lock_panel_win();
        RESIZE_ANIM_SEQ.fetch_add(1, Ordering::SeqCst);
        RESIZE_ANIM_TARGET.store(0, Ordering::SeqCst);
        set_window_region(window, target_h, target_h, scale);
        return;
    }
    // 目标防抖：动画进行中收到与在播目标差 < 4 逻辑 px 的新目标 → 忽略（不顶序号、不重播）
    if should_debounce_target(RESIZE_ANIM_TARGET.load(Ordering::SeqCst), target_h) {
        return;
    }
    let seq = RESIZE_ANIM_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    RESIZE_ANIM_TARGET.store(target_h.to_bits(), Ordering::SeqCst);
    let win = window.clone();
    tauri::async_runtime::spawn(async move {
        let start = std::time::Instant::now();
        let frame = std::time::Duration::from_millis(RESIZE_FRAME_MS);
        let mut next_frame = start + frame; // 绝对时刻表（防逐帧漂移）
        let mut frame_idx: u32 = 0;
        loop {
            // 序号检查每帧（原子读，零成本）；可见性查询是主线程往返，每 8 帧
            //（≈128ms）查一次——隐藏检测延迟无观感影响，却把整段动画的同步 getter
            // 往返降到 2~3 次。
            if RESIZE_ANIM_SEQ.load(Ordering::SeqCst) != seq
                || (frame_idx % 8 == 0 && !window_visible(&win))
            {
                clear_anim_target_if_mine(target_h);
                return;
            }
            frame_idx += 1;
            let t = (start.elapsed().as_millis() as f64 / RESIZE_ANIM_MS as f64).min(1.0);
            let h = start_h + (target_h - start_h) * ease_out_back(t);
            // 发帧前再核一次序号（锁内成对）：region 设置与顶号串行，旧帧不会覆盖新结果
            let guard = lock_panel_win();
            if RESIZE_ANIM_SEQ.load(Ordering::SeqCst) != seq {
                drop(guard);
                clear_anim_target_if_mine(target_h);
                return;
            }
            // region 取大：恒包住面板（变高立即到位、变矮保持到终点），
            // 与 CSS 过渡短暂不同步也不会裁掉面板顶部
            set_window_region(&win, h.max(target_h), h, scale);
            drop(guard);
            if t >= 1.0 {
                clear_anim_target_if_mine(target_h);
                break;
            }
            // 自旋等下一帧时刻（绝对时刻表防漂移）：16ms×~15 次/动画，开销可忽略
            while std::time::Instant::now() < next_frame {
                std::hint::spin_loop();
            }
            next_frame += frame;
        }
    });
}

/// 设置变更/内容高度变化后即时重算面板视觉高度（仅面板可见时）。
/// animate=true 时 region 高度走 300ms back-out 缓动（前端 CSS height 同步过渡），false 瞬时到位。
/// 窗口几何固定，只有 region 在变；定位/显示器缩放与 show_panel 同一链路。
pub fn refit_open_panel(app: &AppHandle, animate: bool) {
    let Some(window) = app.get_webview_window("main") else {
        tracing::warn!("panel refit: main window missing");
        return;
    };
    if !window_visible(&window) {
        return; // 隐藏时不重排（下次 show 会按记忆高校准）
    }
    let Some(tray) = app
        .tray_by_id(crate::tray::TRAY_ID)
        .and_then(|tray| tray.rect().ok().flatten())
        .map(|rect| TrayRect::new(rect.position, rect.size))
    else {
        tracing::warn!("panel refit: tray rect unavailable, skip");
        return; // 取不到托盘位置：不动，下次打开面板时自然会校准
    };
    // fit 返回 true：瞬时路径已完成「尺寸 + 位置」、或动画已启动（首帧即原子定位）；
    // 仅尺寸已接近目标 / 取不到配置或显示器时才走 position_panel 兜底（幂等）
    if !fit_panel_to_screen(app, &window, tray, animate) {
        position_panel(app, &window, tray);
    }
}

/// 第二个实例启动时显示主面板：优先按托盘图标当前位置定位，
/// 取不到托盘 rect 时退化为仅 show + focus。
pub fn show_panel_for_second_instance(app: &AppHandle) {
    let tray_rect = app
        .tray_by_id(crate::tray::TRAY_ID)
        .and_then(|tray| tray.rect().ok().flatten())
        .map(|rect| TrayRect::new(rect.position, rect.size));
    match tray_rect {
        Some(tray) => show_panel(app, tray),
        None => {
            if let Some(window) = app.get_webview_window("main") {
                // 托盘 rect 取不到：无法定位/校准尺寸，至少移除旧 region（防截断）
                clear_window_region(&window);
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
    }
}

/// 把面板定位到托盘图标上方、水平居中对齐，并裁剪到所在显示器范围内。
/// 锚定用外框尺寸（`set_position` 是 outer 坐标，与动画帧同一基准）；系统查询只在此处
/// 发生一次，翻转与 clamp 数学走 anchored_panel_pos 纯函数。
fn position_panel(app: &AppHandle, window: &WebviewWindow, tray: TrayRect) {
    // 读不到外框尺寸就不动：绝不能按 0×0 锚定（那会把窗口顶边锚到托盘上方 8px、
    // 主体盖住托盘图标，比保持原位更糟）；下次 show/refit 会重新校准。
    let Some((_, outer)) = window_sizes(window) else {
        return;
    };
    let (win_w, win_h) = (outer.width as i32, outer.height as i32);
    let mon = app
        .monitor_from_point(tray.x as f64, tray.y as f64)
        .ok()
        .flatten()
        .map(|m| monitor_geom(&m));
    let (x, y) = anchored_panel_pos(tray, mon, win_w, win_h);
    let _guard = lock_panel_win();
    // 在播高度动画每帧都会重定位：此处让路，避免「第 0 帧按当前尺寸定位、首帧按
    // 目标尺寸锚定」交叉造成的一帧跳动（动画结束会清 target，后续定位照常）。
    if RESIZE_ANIM_TARGET.load(Ordering::SeqCst) != 0 {
        return;
    }
    let _ = window.set_position(Position::Physical(PhysicalPosition::new(x, y)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 常规 1080p 屏（100% 缩放）：配置高放得下，不压
    #[test]
    fn keeps_config_height_on_normal_screen() {
        assert_eq!(effective_panel_height(830.0, 1080, 1.0), 830.0);
    }

    /// 矮屏（物理高 600px）：压到「屏高 - 2*EDGE_MARGIN」的逻辑高
    #[test]
    fn clamps_to_short_screen() {
        assert_eq!(effective_panel_height(830.0, 600, 1.0), (600 - 16) as f64);
    }

    /// 高缩放比先把显示器物理高换算成逻辑高再比较（150% + 1080px：逻辑可用 720-10.7 ≈ 709.3 → 压）
    #[test]
    fn converts_monitor_height_before_comparing() {
        let h = effective_panel_height(830.0, 1080, 1.5);
        assert!((h - (1080.0 / 1.5 - 16.0 / 1.5)).abs() < 1e-9);
    }

    /// 高缩放比大屏（150% + 2160px）：逻辑可用 ≈ 1429 > 830，不压
    #[test]
    fn keeps_height_on_hidpi_large_screen() {
        assert_eq!(effective_panel_height(830.0, 2160, 1.5), 830.0);
    }

    /// 非整数缩放（125% + 2160px）：逻辑可用 ≈ 1715 > 830，不压
    #[test]
    fn keeps_height_at_fractional_scale() {
        assert_eq!(effective_panel_height(830.0, 2160, 1.25), 830.0);
    }

    /// 极端小屏防御：上限为负也不返回 0
    #[test]
    fn never_returns_zero_on_tiny_screen() {
        assert_eq!(effective_panel_height(830.0, 8, 1.0), 1.0);
    }

    /// 极简模式基准高：开取紧凑高，关取配置高
    #[test]
    fn base_height_switches_on_minimal_mode() {
        assert_eq!(base_panel_height(830.0, true), MINIMAL_PANEL_HEIGHT);
        assert_eq!(base_panel_height(830.0, false), 830.0);
    }

    /// 紧凑高在常规 1080p 屏放得下（不压），切回普通模式恢复配置高
    #[test]
    fn compact_height_fits_normal_screen() {
        assert_eq!(
            effective_panel_height(base_panel_height(830.0, true), 1080, 1.0),
            MINIMAL_PANEL_HEIGHT
        );
        assert_eq!(
            effective_panel_height(base_panel_height(830.0, false), 1080, 1.0),
            830.0
        );
    }

    /// 紧凑高同样受矮屏压高规则约束（300px 屏：可用 284 < 420 → 压到 屏高-2*EDGE_MARGIN）
    #[test]
    fn compact_height_still_clamped_on_short_screen() {
        assert_eq!(
            effective_panel_height(base_panel_height(830.0, true), 300, 1.0),
            (300 - 16) as f64
        );
    }

    /// back-out：端点精确（0→0、1→1），峰值约 1.02（2% 轻微过冲）后回落；
    /// 前段足够陡（力量感），过冲幅度必须克制（>3% 会让内容裁切明显/显抽搐）
    #[test]
    fn ease_out_back_endpoints_and_overshoot() {
        assert_eq!(ease_out_back(0.0), 0.0);
        assert_eq!(ease_out_back(1.0), 1.0);
        let mut peak = 0.0_f64;
        let mut peak_t = 0.0;
        for i in 1..100 {
            let t = i as f64 / 100.0;
            let v = ease_out_back(t);
            if v > peak {
                peak = v;
                peak_t = t;
            }
        }
        assert!(peak > 1.01 && peak < 1.04, "轻微过冲应约 2%: {peak}");
        assert!((0.6..0.8).contains(&peak_t), "峰值时刻应在中后段: {peak_t}");
        // 中前段保持陡峭：25% 时间至少走完 70%
        assert!(ease_out_back(0.25) > 0.70);
        // 过冲后回落，终点精确回到目标
        assert!(ease_out_back(0.9) < peak);
    }

    // ---- 高度缓动帧数学纯函数（消卡顿改造）----

    /// 逻辑高 × 缩放比取整成物理像素：非整数缩放（1.75）消除半像素颤动；0 高保底 1px
    #[test]
    fn frame_height_rounds_to_physical_pixels() {
        assert_eq!(frame_height_px(400.0, 1.0), 400);
        assert_eq!(frame_height_px(400.0, 1.75), 700);
        // 399.7 × 1.5 = 599.55 → 四舍五入 600
        assert_eq!(frame_height_px(399.7, 1.5), 600);
        assert_eq!(frame_height_px(0.4, 1.0), 1);
    }

    fn tray(x: i32, y: i32, width: i32, height: i32) -> TrayRect {
        TrayRect {
            x,
            y,
            width,
            height,
        }
    }

    fn mon(x: i32, y: i32, w: i32, h: i32) -> Option<MonitorGeom> {
        Some(MonitorGeom { x, y, w, h })
    }

    /// 常规（任务栏在底）：窗口底边锚定托盘上方，水平居中对齐托盘图标
    #[test]
    fn anchored_pos_above_tray_on_bottom_taskbar() {
        // 托盘图标 (900,1000) 40×40，窗口 360×500：x = 900+20-180 = 740，y = 1000-500-8 = 492
        assert_eq!(
            anchored_panel_pos(tray(900, 1000, 40, 40), mon(0, 0, 1920, 1080), 360, 500),
            (740, 492)
        );
    }

    /// 任务栏在顶（托盘贴屏幕顶）：上方放不下 → 翻转到托盘图标下方
    #[test]
    fn anchored_pos_flips_below_when_taskbar_on_top() {
        // y = 0-500-8 = -508 < 8 → 翻转：y = 0+40+8 = 48
        assert_eq!(
            anchored_panel_pos(tray(900, 0, 40, 40), mon(0, 0, 1920, 1080), 360, 500),
            (740, 48)
        );
    }

    /// 托盘贴屏幕右缘：x clamp 进显示器（max_x = 1920-360-8 = 1552）
    #[test]
    fn anchored_pos_clamps_at_screen_right_edge() {
        // 裸 x = 1900+10-180 = 1730 > 1552 → clamp 到 1552
        assert_eq!(
            anchored_panel_pos(tray(1900, 1000, 20, 40), mon(0, 0, 1920, 1080), 360, 500),
            (1552, 492)
        );
    }

    /// 翻转随高度中途触发：同一托盘位置，矮窗不翻转、高窗翻转
    /// （动画压高/长高途中每帧纯算术重判的语义来源）
    #[test]
    fn flip_can_trigger_mid_animation_as_height_grows() {
        let t = tray(900, 300, 40, 40);
        let m = mon(0, 0, 1920, 1080);
        // 矮窗：y = 300-200-8 = 92 ≥ 8 → 不翻转
        assert_eq!(anchored_panel_pos(t, m, 360, 200).1, 92);
        // 高窗：y = 300-400-8 = -108 < 8 → 翻转到托盘下方 300+40+8 = 348
        assert_eq!(anchored_panel_pos(t, m, 360, 400).1, 348);
    }

    /// 取不到显示器（mon=None）：不翻转不 clamp，保持托盘锚定原值（可出屏负坐标）
    #[test]
    fn anchored_pos_without_monitor_skips_flip_and_clamp() {
        assert_eq!(
            anchored_panel_pos(tray(900, 0, 40, 40), None, 360, 500),
            (740, -508)
        );
    }

    /// 目标防抖：无在播目标（位模式 0）一律放行；差 < 4 逻辑 px 忽略；≥ 4 顶掉重播
    #[test]
    fn debounce_ignores_close_targets_only_while_in_flight() {
        assert!(!should_debounce_target(0, 500.0)); // 无在播动画：任何目标都放行
        let in_flight = 500.0_f64.to_bits();
        assert!(should_debounce_target(in_flight, 500.0)); // 同目标：忽略
        assert!(should_debounce_target(in_flight, 503.9)); // 差 3.9 < 4：忽略
        assert!(should_debounce_target(in_flight, 496.1)); // 反向差 3.9：忽略
        assert!(!should_debounce_target(in_flight, 504.0)); // 差 4.0：顶掉重播
        assert!(!should_debounce_target(in_flight, 495.9)); // 反向差 4.1：重播
    }
}
