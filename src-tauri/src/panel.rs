use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use tauri::{
    AppHandle, LogicalSize, Manager, PhysicalPosition, PhysicalSize, Position, Size, WebviewWindow,
};

/// 面板边缘与托盘图标 / 屏幕边缘之间保留的间距（物理像素）。
const EDGE_MARGIN: i32 = 8;

/// 极简模式的面板紧凑逻辑高度（只显示页头 + 7天/5小时额度条 + 翻页圆点 + 底栏）。
/// 内容自适应生效后的兜底：前端尚未实测时用它做极简模式的初始高。
const MINIMAL_PANEL_HEIGHT: f64 = 350.0;

/// 内容自适应的最小窗口逻辑高：防极端内容（如只剩页头 + 底栏）把窗口压得失形。
const MIN_PANEL_HEIGHT: f64 = 200.0;

/// 内容驱动的高度缓动动画时长 / 帧间隔（开窗前的 fit 不动画，见 fit 的 animate 参数）。
/// 帧间隔 4ms（60 次原子 SetWindowPos/250ms，与 240Hz 高分屏刷新率对齐；2026-09-08 领导实机对比
/// 瞬时/30Hz/68Hz/100 次各档后拍板 60 次档——100 次档无可见增益且排版调用多 67%）：
/// tokio sleep 在 Windows 被定时器精度（~15.6ms）限到 ~68Hz（实测 13 步/188ms），
/// 故帧 pacing 用自旋等绝对时刻表——自旋总量 4ms×~60 次/动画、仅沉降/静息收矮触发
const RESIZE_ANIM_MS: u64 = 250;
const RESIZE_FRAME_MS: u64 = 4;
/// 高度动画序号（单调递增）：新动画顶掉进行中的旧动画（可中断，从实时高度起步）
static RESIZE_ANIM_SEQ: AtomicU64 = AtomicU64::new(0);
/// 在播动画的目标逻辑高（f64 位模式；0 = 无在播动画），目标防抖用
static RESIZE_ANIM_TARGET: AtomicU64 = AtomicU64::new(0);
/// 目标防抖阈值（逻辑像素）：动画进行中收到与在播目标差小于此值的新目标 → 忽略
/// （不顶序号、不重启动画），吸收内容测高的亚像素级抖动
const RESIZE_TARGET_DEBOUNCE: f64 = 4.0;

/// ease-out cubic 缓动：起步快、收尾稳
fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
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

/// 窗口外框与客户区在宽/高上的差值（物理像素）。
/// Windows 无边框 + 阴影窗口带隐藏偏移（tao `undecorated_with_shadows`），因此
/// `set_size`（inner 语义）与 `set_position`（outer 语义）不能混用同一套尺寸：
/// 锚定与 SetWindowPos 的 cx/cy 一律用外框尺寸，inner 目标值只在算外框时加回差值。
fn outer_frame_extra(window: &WebviewWindow) -> (i32, i32) {
    let (Ok(inner), Ok(outer)) = (window.inner_size(), window.outer_size()) else {
        return (0, 0);
    };
    (
        outer.width.saturating_sub(inner.width) as i32,
        outer.height.saturating_sub(inner.height) as i32,
    )
}

/// 面板窗口尺寸 + 位置统一出口（物理像素）：
/// - Windows 走单条 `SetWindowPos(hwnd, x, y, outer_w, outer_h)`，位置与尺寸原子生效，
///   消除「先 set_size（顶边锚定向下长出）再 set_position（上移做底边锚定）」两条异步
///   消息之间的错位中间态（切到高页时底边会短暂冲过托盘图标，跨 vblank 即渲染成跳动）；
/// - 拿不到 HWND / 非 Windows 回退 Tauri 两步 API（语义等价，仅多一条消息）。
///
/// `inner_w/inner_h` 是客户区目标尺寸，`extra` 是实测外框差（见 `outer_frame_extra`）；
/// 调用方必须先持 `PANEL_WIN_LOCK`（锁内成对完成「查序号/顶号 + 设置」）。
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
#[cfg(windows)]
fn set_window_size_pos_once(window: &WebviewWindow, x: i32, y: i32, w_px: i32, h_px: i32) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOP, SWP_NOACTIVATE, SWP_NOZORDER,
    };
    let Ok(hwnd) = window.hwnd() else {
        return false;
    };
    // SAFETY: hwnd 为本进程活动窗口句柄；SWP_NOZORDER 不动 Z 序（alwaysOnTop 由窗口
    // 样式保持，不受影响）、SWP_NOACTIVATE 不抢焦点；x/y/cx/cy 均为调用方钳进屏幕
    // 范围的物理像素值。
    unsafe {
        SetWindowPos(
            hwnd.0,
            HWND_TOP,
            x,
            y,
            w_px.max(1),
            h_px.max(1),
            SWP_NOZORDER | SWP_NOACTIVATE,
        ) != 0
    }
}

/// 左键点击托盘图标：切换主面板显隐；显示前先定位到托盘图标上方。
pub fn toggle_panel(app: &AppHandle, tray: TrayRect) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
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
    // 开窗前校准必须瞬时（面板一出现就是对的尺寸，不播动画）
    fit_panel_to_screen(app, &window, tray, false);
    position_panel(app, &window, tray);
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

/// 每次显示面板前校准窗口尺寸：宽恒取配置值，高优先取前端实测内容高（AppState 里的
/// 面板内容高度，自适应主路径），未实测过时回退到基准高（极简紧凑高/配置高），
/// 并在显示器可用高度内取小（小屏压高）。
/// 全程只用逻辑像素 + 托盘所在显示器的缩放比：此时窗口可能还停在别的显示器上，
/// `window.scale_factor()` 是错的；也不把量到的窗口尺寸直接喂回 set_size（set_size 是
/// inner 语义、回读值语义混用会累加偏差把窗口越撑越宽，面板两侧露白边）——尺寸仍从
/// conf/内容高推导，回读值只用于算外框与客户区的固定差（见 `outer_frame_extra`）。
fn fit_panel_to_screen(app: &AppHandle, window: &WebviewWindow, tray: TrayRect, animate: bool) {
    let Some(conf) = app.config().app.windows.iter().find(|w| w.label == "main") else {
        return;
    };
    let Ok(Some(monitor)) = app.monitor_from_point(tray.x as f64, tray.y as f64) else {
        return; // 取不到显示器：保持配置尺寸，不压高
    };
    let scale = monitor.scale_factor();
    // 极简模式读当前设置（读失败按普通模式兜底，不压矮）
    let minimal_mode = kimicodebar::storage::load_settings()
        .map(|s| s.minimal_mode)
        .unwrap_or(false);
    // 内容自适应：前端实测内容高优先（含最小高保护），未实测回退基准高
    let base = app
        .state::<crate::commands::AppState>()
        .panel_content_height()
        .map(|h| h.max(MIN_PANEL_HEIGHT))
        .unwrap_or_else(|| base_panel_height(conf.height, minimal_mode));
    let target_h = effective_panel_height(base, monitor.size().height, scale);
    // 已接近目标尺寸就不动（当前尺寸按窗口自己所在显示器的缩放比换算）
    let cur_scale = window.scale_factor().unwrap_or(scale);
    if let Ok(inner) = window.inner_size() {
        let cur_w = inner.width as f64 / cur_scale;
        let cur_h = inner.height as f64 / cur_scale;
        if (cur_w - conf.width).abs() < 2.0 && (cur_h - target_h).abs() < 2.0 {
            return;
        }
    }
    // animate=false（开窗前校准 / 切页期高度冻结的瞬时到位）一次性到位；
    // animate=true（开窗后内容驱动）走缓动动画
    if !animate {
        // 瞬时到位必须先作废在播的高度动画：否则在播缓动的后续帧会把瞬时结果覆盖回去
        // （issue #48 第二轮实机取证抓到：上一翻沉降触发的收矮缓动尾帧把下一翻的
        // 瞬时长高拖回矮值，高页被裁着滑完、沉降后再弹回——窗口高度乱抖的卡顿本体）。
        // 顶号与设置在同一临界区：原子 SetWindowPos 绕过消息队列 FIFO，旧帧晚到会覆盖
        // 正确结果；锁内先顶号，旧帧下一帧复查序号必退出。
        // 尺寸 + 位置走单条 SetWindowPos 原子生效，锚定用外框尺寸（set_position 是
        // outer 坐标），与动画帧同一基准，消除「长高走瞬时、收矮走动画」各跳一个边框差。
        let w_px = frame_height_px(conf.width, scale) as i32;
        let h_px = frame_height_px(target_h, scale) as i32;
        let extra = outer_frame_extra(window);
        let (x, y) = anchored_panel_pos(
            tray,
            Some(monitor_geom(&monitor)),
            w_px + extra.0,
            h_px + extra.1,
        );
        let _guard = lock_panel_win();
        RESIZE_ANIM_SEQ.fetch_add(1, Ordering::SeqCst);
        RESIZE_ANIM_TARGET.store(0, Ordering::SeqCst);
        apply_inner_size_pos(window, x, y, w_px, h_px, extra);
        return;
    }
    animate_height_to(app, window, tray, conf.width, target_h);
}

/// 把窗口高度从当前实时值缓动到目标值（ease-out cubic 250ms @ 30Hz，底边锚定每帧重定位）。
/// 新的调用顶掉进行中的动画（序号失效即退出，下一次从实时高度重新起步，可中断）；
/// 与在播目标差 < 4 逻辑 px 的新目标被防抖忽略（不顶号、不重启，吸收测高抖动）；
/// 面板中途被隐藏则让出（下次 show 时 fit 会一次性校准）。
/// 消卡顿：托盘 rect / 显示器几何与缩放比 / 起点高 / 外框差值在启动时快照一次，循环体内
/// 零系统查询——每帧只剩纯数学（缓动 + 物理取整 + 锚定翻转/clamp）+ 一次原子
/// SetWindowPos（尺寸与位置同条消息，无「先 resize 再 move」的错位中间态）；
/// 物理像素取整消除亚像素颤动。
fn animate_height_to(
    app: &AppHandle,
    window: &WebviewWindow,
    tray: TrayRect,
    width: f64,
    target_h: f64,
) {
    // 启动时快照：托盘所在显示器的几何与缩放比（fit 已保证取得到；取不到则瞬时到位兜底）
    let Ok(Some(monitor)) = app.monitor_from_point(tray.x as f64, tray.y as f64) else {
        // 无显示器信息无法定位，只能设尺寸；同样要顶号，防旧帧把这次结果覆盖回去
        let _guard = lock_panel_win();
        RESIZE_ANIM_SEQ.fetch_add(1, Ordering::SeqCst);
        RESIZE_ANIM_TARGET.store(0, Ordering::SeqCst);
        let _ = window.set_size(Size::Logical(LogicalSize::new(width, target_h)));
        return;
    };
    let scale = monitor.scale_factor();
    let mon = monitor_geom(&monitor);
    // 起点高快照（读值仅此一次，不喂回）
    let start_h = window
        .inner_size()
        .map(|s| s.height as f64 / scale)
        .unwrap_or(target_h);
    // 起点即目标：不动画，宽度顺带兜底校准（原子设置 + 顶号，与瞬时路径同语义）
    if (start_h - target_h).abs() < 2.0 {
        let w_px = frame_height_px(width, scale) as i32;
        let h_px = frame_height_px(target_h, scale) as i32;
        let extra = outer_frame_extra(window);
        let (x, y) = anchored_panel_pos(tray, Some(mon), w_px + extra.0, h_px + extra.1);
        let _guard = lock_panel_win();
        RESIZE_ANIM_SEQ.fetch_add(1, Ordering::SeqCst);
        RESIZE_ANIM_TARGET.store(0, Ordering::SeqCst);
        apply_inner_size_pos(window, x, y, w_px, h_px, extra);
        return;
    }
    // 目标防抖：动画进行中收到与在播目标差 < 4 逻辑 px 的新目标 → 忽略（不顶序号、不重播）
    if should_debounce_target(RESIZE_ANIM_TARGET.load(Ordering::SeqCst), target_h) {
        return;
    }
    let seq = RESIZE_ANIM_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    RESIZE_ANIM_TARGET.store(target_h.to_bits(), Ordering::SeqCst);
    let win = window.clone();
    // 宽度恒定（conf 逻辑宽不变）：物理宽启动时算一次，帧内复用；
    // 外框差值同样快照（阴影边框不随窗口尺寸变），避免帧内读窗口尺寸
    let win_w = frame_height_px(width, scale) as i32;
    let extra = outer_frame_extra(window);
    tauri::async_runtime::spawn(async move {
        let start = std::time::Instant::now();
        let frame = std::time::Duration::from_millis(RESIZE_FRAME_MS);
        let mut next_frame = start + frame; // 绝对时刻表（防逐帧漂移）
        loop {
            if RESIZE_ANIM_SEQ.load(Ordering::SeqCst) != seq || !win.is_visible().unwrap_or(false) {
                clear_anim_target_if_mine(target_h);
                return;
            }
            let t = (start.elapsed().as_millis() as f64 / RESIZE_ANIM_MS as f64).min(1.0);
            let h = start_h + (target_h - start_h) * ease_out_cubic(t);
            let h_px = frame_height_px(h, scale);
            // 底边锚定纯算术：翻转/clamp 用缓存常量每帧重判（翻转可能随高度变化中途触发）；
            // 尺寸一律用外框（inner + extra），与 set_position 的 outer 坐标语义对齐
            let (x, y) =
                anchored_panel_pos(tray, Some(mon), win_w + extra.0, h_px as i32 + extra.1);
            // 发帧前再核一次序号（锁内成对）：原子 SetWindowPos 绕过消息队列 FIFO，
            // 顶部检查到设置之间若被瞬时到位（animate=false 会顶号）抢跑，这一帧就是
            // 覆盖正确结果的「陈帧」（实机抓到：收矮动画最后一帧把预长结果拖回矮值）
            let guard = lock_panel_win();
            if RESIZE_ANIM_SEQ.load(Ordering::SeqCst) != seq {
                drop(guard);
                clear_anim_target_if_mine(target_h);
                return;
            }
            apply_inner_size_pos(&win, x, y, win_w, h_px as i32, extra);
            drop(guard);
            if t >= 1.0 {
                clear_anim_target_if_mine(target_h);
                break;
            }
            // 自旋等下一帧时刻（绝对时刻表防漂移）：tokio sleep 被 Windows 定时器精度
            // （~15.6ms）限到 ~68Hz 拉不满 240Hz；自旋总量 4ms×~60 次/动画，开销可忽略
            while std::time::Instant::now() < next_frame {
                std::hint::spin_loop();
            }
            next_frame += frame;
        }
    });
}

/// 设置变更/内容高度变化后即时重算面板尺寸并重定位（仅面板可见时）。
/// animate=true 时高度变化走 250ms 缓动动画（内容驱动的翻页收放），false 瞬时到位。
/// 尺寸与定位走与 show_panel 完全相同的链路：conf 逻辑像素 + 托盘所在显示器缩放比。
pub fn refit_open_panel(app: &AppHandle, animate: bool) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if !window.is_visible().unwrap_or(false) {
        return;
    }
    let Some(tray) = app
        .tray_by_id(crate::tray::TRAY_ID)
        .and_then(|tray| tray.rect().ok().flatten())
        .map(|rect| TrayRect::new(rect.position, rect.size))
    else {
        return; // 取不到托盘位置：不动，下次打开面板时自然会校准
    };
    fit_panel_to_screen(app, &window, tray, animate);
    // 瞬时路径已在 fit 内原子完成「尺寸 + 位置」；这里再定位一次是幂等兜底
    // （尺寸未变 / 动画在播时 position_panel 会自行让路，不会与首帧交叉）
    position_panel(app, &window, tray);
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
    let Ok(size) = window.outer_size() else {
        return;
    };
    let (win_w, win_h) = (size.width as i32, size.height as i32);
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

    /// ease-out cubic：端点精确（0→0、1→1），区间内单调递增且始终快于线性（缓出特征）
    #[test]
    fn ease_out_cubic_endpoints_and_shape() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        let mut prev = 0.0;
        for i in 1..=10 {
            let t = i as f64 / 10.0;
            let v = ease_out_cubic(t);
            assert!(v > prev, "应单调递增: {prev} -> {v}");
            assert!(v >= t, "缓出曲线应不落后于线性: {v} >= {t}");
            prev = v;
        }
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
