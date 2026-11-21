#include "vitral_tokens.hpp"
#include <algorithm>
#include <cmath>
#include <d2d1.h>
#include <d3d11.h>
#include <dcomp.h>
#include <dwmapi.h>
#include <dwrite.h>
#include <msctf.h>
#include <string>
#include <vector>
#include <windows.h>
#include <wrl/client.h>

using Microsoft::WRL::ComPtr;
using namespace vitral::tokens;
namespace {
const CLSID clsid = {0x7c9e6679,
                     0x7425,
                     0x40de,
                     {0x94, 0x4b, 0xe0, 0x7f, 0xc1, 0xf9, 0x0a, 0xe7}};
const GUID profile = {0x4aabb15d,
                      0x42ca,
                      0x4d5e,
                      {0xab, 0x90, 0x67, 0xef, 0x1c, 0xd9, 0xe2, 0x02}};
constexpr wchar_t key[] =
    L"SOFTWARE\\Classes\\CLSID\\{7C9E6679-7425-40DE-944B-E07FC1F90AE7}";
HMODULE module() {
  HMODULE h = nullptr;
  GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                         GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                     reinterpret_cast<LPCWSTR>(&module), &h);
  return h;
}
HRESULT setString(HKEY h, const wchar_t *name, const wchar_t *value) {
  return HRESULT_FROM_WIN32(
      RegSetValueExW(h, name, 0, REG_SZ, reinterpret_cast<const BYTE *>(value),
                     (DWORD)(wcslen(value) + 1) * 2));
}
HRESULT reg(bool install) {
  ComPtr<ITfInputProcessorProfileMgr> profiles;
  ComPtr<ITfCategoryMgr> categories;
  HRESULT hr = CoCreateInstance(CLSID_TF_InputProcessorProfiles, nullptr,
                                CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&profiles));
  if (FAILED(hr))
    return hr;
  hr = CoCreateInstance(CLSID_TF_CategoryMgr, nullptr, CLSCTX_INPROC_SERVER,
                        IID_PPV_ARGS(&categories));
  if (FAILED(hr))
    return hr;
  if (!install) {
    HRESULT p = profiles->UnregisterProfile(clsid, 0x0804, profile, 0);
    HRESULT c =
        categories->UnregisterCategory(clsid, GUID_TFCAT_TIP_KEYBOARD, clsid);
    LONG del = RegDeleteTreeW(HKEY_LOCAL_MACHINE, key);
    if (del != ERROR_SUCCESS && del != ERROR_FILE_NOT_FOUND)
      return HRESULT_FROM_WIN32(del);
    return FAILED(p) ? p : c;
  }
  wchar_t path[32768];
  DWORD n = GetModuleFileNameW(module(), path, 32768);
  if (!n || n >= 32768)
    return E_FAIL;
  HKEY h;
  auto sub = std::wstring(key) + L"\\InprocServer32";
  LONG rc = RegCreateKeyExW(HKEY_LOCAL_MACHINE, sub.c_str(), 0, nullptr, 0,
                            KEY_WRITE, nullptr, &h, nullptr);
  if (rc != ERROR_SUCCESS)
    return HRESULT_FROM_WIN32(rc);
  hr = setString(h, nullptr, path);
  if (SUCCEEDED(hr))
    hr = setString(h, L"ThreadingModel", L"Apartment");
  RegCloseKey(h);
  if (SUCCEEDED(hr))
    hr = categories->RegisterCategory(clsid, GUID_TFCAT_TIP_KEYBOARD, clsid);
  if (SUCCEEDED(hr))
    hr = profiles->RegisterProfile(clsid, 0x0804, profile, L"Vitral", 6, path,
                                   n, 0, nullptr, 0, TRUE, 0);
  if (FAILED(hr)) {
    categories->UnregisterCategory(clsid, GUID_TFCAT_TIP_KEYBOARD, clsid);
    RegDeleteTreeW(HKEY_LOCAL_MACHINE, key);
  }
  return hr;
}
using Event = void (*)(void *, int, int, const wchar_t *, const float *,
                       size_t);
struct Panel;
struct Surface {
  Panel *panel;
  bool status;
  HWND wnd = nullptr;
  ComPtr<IDCompositionTarget> target;
  ComPtr<IDCompositionVisual> visual;
  ComPtr<IDCompositionSurface> surface;
  void show(int x, int y, int width, int height);
  void paint();
};
struct Panel {
  Event event;
  void *owner;
  Surface status{this, true}, choices{this, false};
  ComPtr<ID3D11Device> gpu;
  ComPtr<IDCompositionDevice> compositor;
  ComPtr<ID2D1Factory> d2d;
  ComPtr<IDWriteFactory> write;
  ComPtr<IDWriteTextFormat> font;
  bool active = false, english = false, paste = false, ink = false,
       drawing = false;
  bool idle = false;
  int count = 0, mode = 0;
  RECT caret{};
  ULONGLONG last = GetTickCount64(), glow = 0;
  std::wstring words[5];
  std::vector<float> points;
  void send(int op, int index = 0, const wchar_t *text = nullptr) {
    event(owner, op, index, text, nullptr, 0);
  }
  void place();
  void init() {
    D3D_FEATURE_LEVEL level;
    HRESULT h = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr,
                                  D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                                  D3D11_SDK_VERSION, &gpu, &level, nullptr);
    if (FAILED(h))
      D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_WARP, nullptr,
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                        D3D11_SDK_VERSION, &gpu, &level, nullptr);
    ComPtr<IDXGIDevice> dx;
    if (gpu && SUCCEEDED(gpu.As(&dx)))
      DCompositionCreateDevice(dx.Get(), IID_PPV_ARGS(&compositor));
    D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, d2d.GetAddressOf());
    DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED, __uuidof(IDWriteFactory),
                        reinterpret_cast<IUnknown **>(write.GetAddressOf()));
    if (write) {
      write->CreateTextFormat(L"Microsoft YaHei UI", nullptr,
                              DWRITE_FONT_WEIGHT_NORMAL,
                              DWRITE_FONT_STYLE_NORMAL,
                              DWRITE_FONT_STRETCH_NORMAL, 16, L"zh-CN", &font);
      if (font) {
        font->SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
        font->SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
      }
    }
  }
};
void Surface::paint() {
  if (!wnd || !visual || !panel->d2d || !panel->font)
    return;
  RECT r;
  GetClientRect(wnd, &r);
  int w = r.right, h = r.bottom;
  if (!w || !h)
    return;
  ComPtr<IDCompositionSurface> s;
  if (FAILED(panel->compositor->CreateSurface(
          w, h, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ALPHA_MODE_PREMULTIPLIED, &s)))
    return;
  POINT origin;
  ComPtr<IDXGISurface> dx;
  if (FAILED(s->BeginDraw(nullptr, IID_PPV_ARGS(&dx), &origin)))
    return;
  ComPtr<ID2D1RenderTarget> rt;
  auto props = D2D1::RenderTargetProperties(
      D2D1_RENDER_TARGET_TYPE_DEFAULT,
      D2D1::PixelFormat(DXGI_FORMAT_B8G8R8A8_UNORM,
                        D2D1_ALPHA_MODE_PREMULTIPLIED),
      96, 96);
  if (SUCCEEDED(
          panel->d2d->CreateDxgiSurfaceRenderTarget(dx.Get(), props, &rt))) {
    rt->BeginDraw();
    rt->SetTransform(
        D2D1::Matrix3x2F::Translation((float)origin.x, (float)origin.y));
    rt->Clear(D2D1::ColorF(0, 0.f));
    ComPtr<ID2D1SolidColorBrush> b;
    rt->CreateSolidColorBrush(D2D1::ColorF(1, 1, 1), &b);
    if (b) {
      auto color = [&](RgbaColor c) {
        b->SetColor(
            D2D1::ColorF(c.r / 255.f, c.g / 255.f, c.b / 255.f, c.alpha));
      };
      float rad =
          status ? (std::min)((float)radius::kDesktopStatusCapsulePx, h / 2.f)
                 : radius::kDesktopCandidateWindowPx;
      auto rr = D2D1::RoundedRect(D2D1::RectF(1, 1, (float)w - 1, (float)h - 1),
                                  rad, rad);
      color(kDarkTheme.surface_panel);
      rt->FillRoundedRectangle(rr, b.Get());
      color(kDarkTheme.border_specular);
      rt->DrawRoundedRectangle(rr, b.Get());
      auto label = [&](const std::wstring &text, float x, float end) {
        color(kDarkTheme.text_primary);
        rt->DrawText(text.data(), (UINT32)text.size(), panel->font.Get(),
                     D2D1::RectF(x, 0, end, 40), b.Get(),
                     D2D1_DRAW_TEXT_OPTIONS_CLIP);
      };
      if (status) {
        color(kDarkTheme.accent_primary);
        rt->DrawLine({12, 13}, {20, 28}, b.Get(), 3);
        rt->DrawLine({20, 28}, {28, 13}, b.Get(), 3);
        label(panel->english ? L"En" : L"中", 37, 75);
        if (panel->paste)
          label(L"粘贴", 75, 135);
      } else {
        for (int i = 0; i < panel->count; i++)
          label(panel->words[i], i * 100.f, (i + 1) * 100.f);
        if (panel->ink) {
          color(kDarkTheme.ink_stroke_primary);
          auto &p = panel->points;
          for (size_t i = 3; i + 2 < p.size(); i += 3)
            rt->DrawLine({p[i - 3] * w, 42 + p[i - 2] * (h - 42)},
                         {p[i] * w, 42 + p[i + 1] * (h - 42)}, b.Get(), 3);
        }
        if (GetTickCount64() < panel->glow) {
          color(kDarkTheme.retroactive_fix_glow);
          rt->DrawLine({10, 39}, {(float)w - 10, 39}, b.Get(), 2);
        }
      }
    }
    rt->EndDraw();
  }
  s->EndDraw();
  visual->SetContent(s.Get());
  surface = s;
  ComPtr<IDCompositionEffectGroup> alpha;
  if (SUCCEEDED(panel->compositor->CreateEffectGroup(&alpha))) {
    alpha->SetOpacity(status && panel->idle
                          ? motion::kDesktopIdleOpacity
                          : 1.f);
    visual->SetEffect(alpha.Get());
  }
  panel->compositor->Commit();
}
void Surface::show(int x, int y, int w, int h) {
  if (!wnd)
    return;
  RECT old{};
  GetWindowRect(wnd, &old);
  if (!IsWindowVisible(wnd) || old.left != x || old.top != y ||
      old.right - old.left != w || old.bottom - old.top != h) {
    SetWindowPos(wnd, HWND_TOPMOST, x, y, w, h,
                 SWP_NOACTIVATE | SWP_SHOWWINDOW);
    InvalidateRect(wnd, nullptr, FALSE);
  }
}
void Panel::place() {
  auto fg = GetForegroundWindow();
  MONITORINFO mi{sizeof(mi)};
  GetMonitorInfoW(MonitorFromWindow(fg, MONITOR_DEFAULTTOPRIMARY), &mi);
  RECT fgbox{};
  GetWindowRect(fg, &fgbox);
  if (!active || !fg || EqualRect(&mi.rcMonitor, &fgbox)) {
    ShowWindow(status.wnd, SW_HIDE);
    ShowWindow(choices.wnd, SW_HIDE);
    return;
  }
  auto a = mi.rcWork;
  int sw = paste ? 138 : 82;
  POINT mouse{};
  GetCursorPos(&mouse);
  bool was_idle = idle;
  idle = GetTickCount64() - last >= motion::kDesktopIdleFadeDelayMs;
  if (idle != was_idle && status.wnd)
    InvalidateRect(status.wnd, nullptr, FALSE);
  bool edge =
      idle && !(mouse.x >= a.right - sw - 12 && mouse.y >= a.bottom - 65);
  status.show(edge ? a.right - sw / 2 : a.right - sw - 12, a.bottom - 52, sw,
              40);
  if (!ink && !count) {
    ShowWindow(choices.wnd, SW_HIDE);
    return;
  }
  int w = ink ? 500 : (std::max)(100, count * 100), h = ink ? 208 : 42;
  int x = caret.right || caret.bottom ? caret.left : a.right - w - 14;
  int y = caret.bottom ? caret.bottom + 3 : a.bottom - h - 65;
  if (y + h > a.bottom)
    y = caret.top - h - 3;
  choices.show(
      std::clamp(x, (int)a.left, (std::max)((int)a.left, (int)a.right - w)),
      std::clamp(y, (int)a.top, (std::max)((int)a.top, (int)a.bottom - h)), w,
      h);
}
LRESULT CALLBACK procedure(HWND win, UINT msg, WPARAM wp, LPARAM lp) {
  auto s = reinterpret_cast<Surface *>(GetWindowLongPtrW(win, GWLP_USERDATA));
  if (msg == WM_NCCREATE) {
    s = (Surface *)((CREATESTRUCT *)lp)->lpCreateParams;
    SetWindowLongPtrW(win, GWLP_USERDATA, (LONG_PTR)s);
    s->wnd = win;
  }
  if (!s)
    return DefWindowProcW(win, msg, wp, lp);
  auto &p = *s->panel;
  int x = (short)LOWORD(lp), y = (short)HIWORD(lp);
  switch (msg) {
  case WM_ERASEBKGND:
    return 1;
  case WM_PAINT: {
    PAINTSTRUCT ps;
    BeginPaint(win, &ps);
    s->paint();
    EndPaint(win, &ps);
    return 0;
  }
  case WM_MOUSEACTIVATE:
    return MA_NOACTIVATE;
  case WM_TIMER:
    if (wp == 2) {
      KillTimer(win, 2);
      InvalidateRect(win, nullptr, FALSE);
    } else if (p.active) {
      p.send(8);
      p.place();
    }
    return 0;
  case WM_CLIPBOARDUPDATE:
    if (p.active && OpenClipboard(win)) {
      HANDLE h = GetClipboardData(CF_UNICODETEXT);
      std::wstring text;
      if (h) {
        auto data = (const wchar_t *)GlobalLock(h);
        if (data) {
          size_t n = wcsnlen(data, GlobalSize(h) / sizeof(wchar_t));
          text.assign(data, n);
          GlobalUnlock(h);
        }
      }
      CloseClipboard();
      if (!text.empty())
        p.send(9, 0, text.c_str());
    }
    return 0;
  case WM_RBUTTONUP: {
    auto menu = CreatePopupMenu();
    AppendMenuW(menu, MF_STRING, 1, L"手写查音");
    AppendMenuW(menu, MF_STRING, 2, L"连续手写");
    AppendMenuW(menu, MF_STRING, 3, L"确认手写");
    AppendMenuW(menu, MF_STRING, 4, L"关闭手写");
    AppendMenuW(menu, MF_STRING, 5, L"打开 Vitral 设置");
    POINT pt{x, y};
    ClientToScreen(win, &pt);
    int id = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY, pt.x, pt.y, 0,
                            win, nullptr);
    DestroyMenu(menu);
    if (id) {
      p.last = GetTickCount64();
      p.send(3, id);
    }
    return 0;
  }
  case WM_LBUTTONDOWN: {
    p.last = GetTickCount64();
    if (s->status) {
      if (x < 36)
        PostMessageW(win, WM_RBUTTONUP, 0, lp);
      else
        p.send(x > 75 && p.paste ? 2 : 1);
      return 0;
    }
    if (y <= 42) {
      if (x / 100 < p.count)
        p.send(0, x / 100);
      return 0;
    }
    if (p.ink) {
      p.points.clear();
      p.drawing = true;
      SetCapture(win);
    }
    break;
  }
  case WM_CAPTURECHANGED:
    p.drawing = false;
    return 0;
  }
  if (!s->status && p.drawing &&
      (msg == WM_LBUTTONDOWN || msg == WM_MOUSEMOVE || msg == WM_LBUTTONUP)) {
    RECT r;
    GetClientRect(win, &r);
    p.points.insert(p.points.end(),
                    {std::clamp(float(x) / r.right, 0.f, 1.f),
                     std::clamp(float(y - 42) / (r.bottom - 42), 0.f, 1.f),
                     float(GetTickCount64() % 600000)});
    InvalidateRect(win, nullptr, FALSE);
    if (msg == WM_LBUTTONUP) {
      p.drawing = false;
      ReleaseCapture();
      p.event(p.owner, 4, 0, nullptr, p.points.data(), p.points.size() / 3);
    }
    return 0;
  }
  return DefWindowProcW(win, msg, wp, lp);
}
} // namespace
extern "C" HRESULT vitral_register(bool install) {
  HRESULT init = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
  if (FAILED(init) && init != RPC_E_CHANGED_MODE)
    return init;
  HRESULT hr = reg(install);
  if (SUCCEEDED(init))
    CoUninitialize();
  return hr;
}
extern "C" unsigned vitral_module_path(wchar_t *path, unsigned size) {
  return GetModuleFileNameW(module(), path, size);
}
extern "C" void *vitral_panel_open(void *owner, Event event) {
  auto p = new (std::nothrow) Panel();
  if (!p)
    return nullptr;
  p->owner = owner;
  p->event = event;
  p->init();
  WNDCLASSW wc{};
  wc.lpfnWndProc = procedure;
  wc.hInstance = module();
  wc.lpszClassName = L"Vitral.Tsf.Panel";
  wc.hCursor = LoadCursorW(nullptr, IDC_ARROW);
  RegisterClassW(&wc);
  for (auto s : {&p->status, &p->choices}) {
    HWND win = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST |
                                   WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
                               wc.lpszClassName, L"Vitral", WS_POPUP, 0, 0, 100,
                               40, nullptr, nullptr, module(), s);
    if (!win)
      continue;
    int backdrop = 3;
    DwmSetWindowAttribute(win, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop,
                          sizeof(backdrop));
    if (p->compositor &&
        SUCCEEDED(p->compositor->CreateTargetForHwnd(win, TRUE, &s->target)) &&
        SUCCEEDED(p->compositor->CreateVisual(&s->visual)))
      s->target->SetRoot(s->visual.Get());
  }
  if (p->status.wnd) {
    SetTimer(p->status.wnd, 1, 750, nullptr);
    AddClipboardFormatListener(p->status.wnd);
  }
  return p;
}
extern "C" void vitral_panel_close(Panel *p) {
  if (!p)
    return;
  if (p->status.wnd) {
    RemoveClipboardFormatListener(p->status.wnd);
    DestroyWindow(p->status.wnd);
  }
  if (p->choices.wnd)
    DestroyWindow(p->choices.wnd);
  delete p;
  UnregisterClassW(L"Vitral.Tsf.Panel", module());
}
extern "C" void vitral_panel_update(Panel *p, bool active, bool english,
                                    bool paste, int mode, const RECT *caret,
                                    const wchar_t *const *words, int count,
                                    bool glow, bool activity) {
  if (!p)
    return;
  p->active = active;
  p->english = english;
  p->paste = paste;
  p->ink = mode != 0;
  p->mode = mode;
  p->count = (std::min)(5, count);
  if (caret)
    p->caret = *caret;
  for (int i = 0; i < p->count; i++)
    p->words[i] = words[i];
  if (glow && p->choices.wnd) {
    p->glow = GetTickCount64() + handwriting::kRetroactiveHighlightDurationMs;
    SetTimer(p->choices.wnd, 2, handwriting::kRetroactiveHighlightDurationMs, nullptr);
  }
  if (activity)
    p->last = GetTickCount64();
  p->place();
  if (p->status.wnd)
    InvalidateRect(p->status.wnd, nullptr, FALSE);
  if (p->choices.wnd)
    InvalidateRect(p->choices.wnd, nullptr, FALSE);
}
