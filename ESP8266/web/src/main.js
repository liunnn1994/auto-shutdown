// ESP8266 管理界面入口：htmx + 样式 + 页面行为
// 注意兼容性：本页面会在配网弹出的 WebView 中打开，避免使用过新的 API
// （详见 src/style.css 顶部的兼容性说明）
import htmx from "htmx.org";
window.htmx = htmx;
import "./style.css";

// ---------------------------------------------------------------------------
// 配网窗口兼容：登录跳转直接到达的页面在该窗口里可能无法滚动，
// 登录成功后带 postlogin=1 参数跳回，这里检测到就再加载一次干净的 "/"。
// 用 URL 参数而非 storage：配网 WebView 可能运行在无存储模式。
// ---------------------------------------------------------------------------
if (location.search.indexOf("postlogin=1") >= 0) {
  location.replace(location.pathname);
}

var $ = function (id) {
  return document.getElementById(id);
};

// ---------------------------------------------------------------------------
// WiFi SSID 选择：扫描下拉（select）与手动输入（input）都同步到表单隐藏域
// #ssid，用 document 级事件委托，对 htmx 动态换入的扫描结果同样生效
// ---------------------------------------------------------------------------

function setSsid(v) {
  var el = $("ssid");
  if (!el) return;
  el.value = v;
  var hint = $("ssid-hint");
  if (hint) {
    hint.textContent = v ? "当前选择的网络：" + v : "";
    hint.classList.toggle("hidden", !v);
  }
}

// 初始提示（固件会把 __SSID__ 替换为当前已配置的 SSID）
document.addEventListener("DOMContentLoaded", function () {
  var v = ssidValue();
  if (v) setSsid(v);
});

function ssidValue() {
  var el = $("ssid");
  return el ? el.value : "";
}

document.addEventListener("change", function (e) {
  if (e.target.id === "wifi-select" && e.target.value) setSsid(e.target.value);
});

document.addEventListener("input", function (e) {
  if (e.target.id === "wifi-manual") setSsid(e.target.value.trim());
});

// ---------------------------------------------------------------------------
// 登录页：提交时显示全屏 loading（登录是普通表单整页跳转）
// ---------------------------------------------------------------------------

var loginForm = document.querySelector("#login-form");
if (loginForm) {
  loginForm.addEventListener("submit", function () {
    var busy = $("login-busy");
    if (busy) busy.style.display = "flex";
  });
}

// ---------------------------------------------------------------------------
// 配网窗口反下拉刷新：document 钉在 scrollTop=1（配合 style.css 的
// body 高度余量），让系统的 SwipeRefreshLayout 永远认为"页面可以向上滚"，
// 不再抢走下拉手势触发整页刷新。正常浏览器里此操作不可见（只有 1px）。
// ---------------------------------------------------------------------------

function pinDocumentScroll() {
  if (window.scrollY < 1) window.scrollTo(0, 1);
}
window.addEventListener("load", pinDocumentScroll);
window.addEventListener("scroll", pinDocumentScroll);
setInterval(pinDocumentScroll, 800);

// ---------------------------------------------------------------------------
// 修改密码弹窗（<dialog> 不可用时回退为 open 属性，兼容 daisyUI modal）
// ---------------------------------------------------------------------------

function openPwModal() {
  var m = $("pw-modal");
  if (!m) return;
  if (m.showModal) m.showModal();
  else m.setAttribute("open", "");
}

function closePwModal() {
  var m = $("pw-modal");
  if (!m) return;
  if (m.close) m.close();
  else m.removeAttribute("open");
}

var menuPw = $("menu-pw");
if (menuPw) {
  menuPw.addEventListener("click", function () {
    openPwModal();
    if (document.activeElement) document.activeElement.blur();
  });
}

var pwCancel = $("pw-cancel");
if (pwCancel) pwCancel.addEventListener("click", closePwModal);

// 新密码二次输入校验：不一致时显示错误并禁用保存按钮（实时反馈）
function checkPwMatch() {
  var nw = $("pw-new");
  var cf = $("pw-confirm");
  if (!nw || !cf) return;
  var bad = cf.value.length > 0 && nw.value !== cf.value;
  var err = $("pw-match-err");
  if (err) err.classList.toggle("hidden", !bad);
  var save = $("pw-save");
  if (save) save.disabled = bad;
}
["input", "change"].forEach(function (ev) {
  var nw = $("pw-new");
  var cf = $("pw-confirm");
  if (nw) nw.addEventListener(ev, checkPwMatch);
  if (cf) cf.addEventListener(ev, checkPwMatch);
});

// 弹窗每次打开时清空上次的状态
var pwModal = $("pw-modal");
if (pwModal) {
  pwModal.addEventListener("close", function () {
    var form = $("pw-form");
    if (form) form.reset();
    var err = $("pw-match-err");
    if (err) err.classList.add("hidden");
    var save = $("pw-save");
    if (save) save.disabled = false;
    var result = $("pw-result");
    if (result) result.innerHTML = "";
  });
}
