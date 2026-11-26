#include "vitral_tokens.hpp"
#include <algorithm>
#include <cmath>
#include <cstdint>
#include <d2d1.h>
#include <d3d11.h>
#include <dcomp.h>
#include <dwmapi.h>
#include <dwrite.h>
#include <msctf.h>
#include <string>
#include <string_view>
#include <vector>
#include <windows.h>
#include <wrl/client.h>

using Microsoft::WRL::ComPtr;
using namespace vitral::tokens;
namespace {
class ScopedPanelDpi {
  using SetContext = DPI_AWARENESS_CONTEXT(WINAPI *)(DPI_AWARENESS_CONTEXT);
  static SetContext setContext() {
    static auto fn = reinterpret_cast<SetContext>(GetProcAddress(
        GetModuleHandleW(L"user32.dll"), "SetThreadDpiAwarenessContext"));
    return fn;
  }
  DPI_AWARENESS_CONTEXT previous = nullptr;

public:
  ScopedPanelDpi() {
    if (auto set = setContext()) {
      previous = set(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
      if (!previous)
        previous = set(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
    }
  }
  ~ScopedPanelDpi() {
    if (previous)
      setContext()(previous);
  }
  bool perMonitor() const { return previous != nullptr; }
};

UINT windowDpi(HWND hwnd) {
  using GetDpi = UINT(WINAPI *)(HWND);
  static auto getDpi = reinterpret_cast<GetDpi>(
      GetProcAddress(GetModuleHandleW(L"user32.dll"), "GetDpiForWindow"));
  return getDpi && hwnd ? getDpi(hwnd) : 0;
}

UINT monitorDpi(HMONITOR monitor, HWND fallback) {
  using GetDpi = HRESULT(WINAPI *)(HMONITOR, int, UINT *, UINT *);
  struct ShcoreModule {
    HMODULE handle = LoadLibraryExW(L"Shcore.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    ~ShcoreModule() {
      if (handle)
        FreeLibrary(handle);
    }
  };
  static const ShcoreModule shcore;
  static const auto getDpi = shcore.handle
      ? reinterpret_cast<GetDpi>(GetProcAddress(shcore.handle, "GetDpiForMonitor"))
      : nullptr;
  UINT x = 0, y = 0;
  if (getDpi && SUCCEEDED(getDpi(monitor, 0, &x, &y)) && x)
    return x;
  const UINT dpi = windowDpi(fallback);
  return dpi ? dpi : 96;
}

bool coversMonitor(HWND hwnd, const RECT &monitor) {
  if ((GetWindowLongW(hwnd, GWL_STYLE) & WS_CAPTION) == WS_CAPTION)
    return false;
  RECT bounds{};
  return GetWindowRect(hwnd, &bounds) && bounds.left <= monitor.left &&
         bounds.top <= monitor.top && bounds.right >= monitor.right &&
         bounds.bottom >= monitor.bottom;
}

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
const GUID *const categoriesSupported[] = {
    &GUID_TFCAT_TIP_KEYBOARD,
    &GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
    &GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
    &GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT,
};

const ThemePalette &currentPalette() {
  DWORD value = 0;
  DWORD size = sizeof(value);
  if (RegGetValueW(
          HKEY_CURRENT_USER,
          L"Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize",
          L"AppsUseLightTheme", RRF_RT_REG_DWORD, nullptr, &value, &size) ==
      ERROR_SUCCESS) {
    return value == 1 ? kLightTheme : kDarkTheme;
  }
  return kDarkTheme;
}

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
    HRESULT c = S_OK;
    for (const auto category : categoriesSupported) {
      const HRESULT result = categories->UnregisterCategory(clsid, *category, clsid);
      if (FAILED(result) && SUCCEEDED(c))
        c = result;
    }
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
  for (const auto category : categoriesSupported) {
    if (SUCCEEDED(hr))
      hr = categories->RegisterCategory(clsid, *category, clsid);
  }
  if (SUCCEEDED(hr)) {
    hr = profiles->RegisterProfile(clsid, 0x0804, profile, L"Vitral", 6,
                                   path, n, static_cast<ULONG>(-101),
                                   nullptr, 0, TRUE, 0);
  }
  if (FAILED(hr)) {
    for (const auto category : categoriesSupported)
      categories->UnregisterCategory(clsid, *category, clsid);
    RegDeleteTreeW(HKEY_LOCAL_MACHINE, key);
  }
  return hr;
}
using Event = void (*)(void *, int, int, const wchar_t *, const float *,
                       size_t);
struct PanelText {
  const char *data;
  size_t len;
  std::string_view view() const { return {data, len}; }
};
std::wstring wideText(std::string_view text) {
  const int len = static_cast<int>(text.size());
  const int size =
      MultiByteToWideChar(CP_UTF8, 0, text.data(), len, nullptr, 0);
  std::wstring wide(size, L'\0');
  if (size)
    MultiByteToWideChar(CP_UTF8, 0, text.data(), len, wide.data(), size);
  return wide;
}
struct Panel;
struct Surface {
  Panel *panel;
  bool status;
  HWND wnd = nullptr;
  ComPtr<IDCompositionTarget> target;
  ComPtr<IDCompositionVisual> visual;
  ComPtr<IDCompositionSurface> surface;
  int width = 0, height = 0, selected = -1;
  bool dirty = true, glowing = false, faded = false;
  void redraw() {
    if (wnd)
      InvalidateRect(wnd, nullptr, FALSE);
  }
  void invalidate() {
    dirty = true;
    redraw();
  }
  void show(int x, int y, int width, int height);
  void paint();
};
struct Candidate {
  std::string text;
  std::wstring label;
  ComPtr<IDWriteTextLayout> layout;
  float width = 62.f, left = 0.f, right = 0.f;
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
  ComPtr<IDWriteTextFormat> smallFont;
  bool active = false, english = false, ink = false, drawing = false;
  bool idle = false;
  int count = 0, mode = 0, selected = 0;
  uint64_t page = 0;
  std::string preedit;
  const ThemePalette *palette = &currentPalette();
  RECT caret{};
  UINT dpi = 96;
  float scale = 1.f;
  ULONGLONG last = GetTickCount64(), glow = 0;
  Candidate candidates[5];
  int totalChoiceWidth = 100;
  std::vector<float> points;
  int px(float dip) const { return static_cast<int>(std::lround(dip * scale)); }
  void setDpi(UINT next) {
    if (dpi == next)
      return;
    dpi = next;
    scale = next / 96.f;
    status.invalidate();
    choices.invalidate();
  }
  void send(int op, int index = 0, const wchar_t *text = nullptr) {
    event(owner, op, index, text, nullptr, 0);
  }
  bool updateChoices(const PanelText *words, int newCount) {
    bool changed = count != newCount;
    count = newCount;
    for (int i = 0; i < count; i++) {
      auto &c = candidates[i];
      const auto word = words[i].view();
      if (c.label.empty() || c.text != word) {
        changed = true;
        c.text = word;
        c.label = std::to_wstring(i + 1) + L" " + wideText(word);
        c.width = 62.f;
        c.layout.Reset();
        if (write && font &&
            SUCCEEDED(
                write->CreateTextLayout(c.label.data(), (UINT32)c.label.size(),
                                        font.Get(), 400.f, 38.f, &c.layout))) {
          DWRITE_TEXT_METRICS m{};
          if (SUCCEEDED(c.layout->GetMetrics(&m)))
            c.width =
                (std::max)(56.f, std::ceil(m.widthIncludingTrailingWhitespace) +
                                     24.f);
          c.layout->SetMaxWidth(c.width);
        }
      }
    }
    if (!changed)
      return false;
    float cursor = 8.f;
    for (int i = 0; i < count; i++) {
      auto &c = candidates[i];
      c.left = cursor;
      c.right = cursor + c.width;
      cursor = c.right + 4.f;
    }
    totalChoiceWidth = (std::max)(110, (int)std::ceil(cursor + 4.f));
    return true;
  }
  void refreshTheme() {
    const auto *next = &currentPalette();
    if (palette != next) {
      palette = next;
      status.invalidate();
      choices.invalidate();
    }
  }
  int hitChoice(float x) const {
    for (int i = 0; i < count; i++) {
      if (x >= candidates[i].left && x <= candidates[i].right)
        return i;
    }
    return -1;
  }
  void place();
  void initGraphics() {
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
  }
  void bindSurface(Surface &s) {
    s.surface.Reset();
    s.visual.Reset();
    s.target.Reset();
    s.width = 0;
    s.height = 0;
    s.selected = -1;
    s.faded = false;
    s.dirty = true;
    if (s.wnd && compositor &&
        SUCCEEDED(compositor->CreateTargetForHwnd(s.wnd, TRUE, &s.target)) &&
        SUCCEEDED(compositor->CreateVisual(&s.visual)))
      s.target->SetRoot(s.visual.Get());
  }
  bool recoverGraphics() {
    status.surface.Reset();
    status.visual.Reset();
    status.target.Reset();
    choices.surface.Reset();
    choices.visual.Reset();
    choices.target.Reset();
    compositor.Reset();
    gpu.Reset();
    initGraphics();
    bindSurface(status);
    bindSurface(choices);
    return compositor != nullptr;
  }
  void init() {
    initGraphics();
    D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, d2d.GetAddressOf());
    DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED, __uuidof(IDWriteFactory),
                        reinterpret_cast<IUnknown **>(write.GetAddressOf()));
    if (write) {
      write->CreateTextFormat(L"Microsoft YaHei UI", nullptr,
                              DWRITE_FONT_WEIGHT_NORMAL,
                              DWRITE_FONT_STYLE_NORMAL,
                              DWRITE_FONT_STRETCH_NORMAL, 15.5f, L"zh-CN", &font);
      if (font) {
        font->SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
        font->SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
      }
      write->CreateTextFormat(L"Microsoft YaHei UI", nullptr,
                              DWRITE_FONT_WEIGHT_SEMI_BOLD,
                              DWRITE_FONT_STYLE_NORMAL,
                              DWRITE_FONT_STRETCH_NORMAL, 12.f, L"zh-CN",
                              &smallFont);
      if (smallFont) {
        smallFont->SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
        smallFont->SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
      }
    }
  }
};
void Surface::paint() {
  if (!wnd || !panel->d2d || !panel->font)
    return;
  bool recovered = false;
  auto ensureDevice = [&]() -> bool {
    if (panel->compositor) {
      BOOL valid = FALSE;
      if (SUCCEEDED(panel->compositor->CheckDeviceState(&valid)) && valid &&
          visual)
        return true;
    }
    if (recovered)
      return false;
    recovered = true;
    return panel->recoverGraphics() && visual != nullptr;
  };
  if (!ensureDevice())
    return;
  for (int attempt = 0; attempt < 2; ++attempt) {
  RECT client{};
  if (!GetClientRect(wnd, &client) || IsRectEmpty(&client))
    return;
  const int w = client.right, h = client.bottom;
  const float scale = panel->scale;
  const float dipWidth = w / scale, dipHeight = h / scale;
  const auto &pal = *panel->palette;
  if (status && faded != panel->idle) {
    ComPtr<IDCompositionEffectGroup> alpha;
    if (SUCCEEDED(panel->compositor->CreateEffectGroup(&alpha)) &&
        SUCCEEDED(alpha->SetOpacity(
            panel->idle ? (float)motion::kDesktopIdleOpacity : 1.f)) &&
        SUCCEEDED(visual->SetEffect(alpha.Get())) &&
        SUCCEEDED(panel->compositor->Commit()))
      faded = panel->idle;
  }
  const int nextSelected = !status && !panel->ink && panel->selected >= 0 &&
                                   panel->selected < panel->count
                               ? panel->selected
                               : -1;
  const bool nextGlow = !status && GetTickCount64() < panel->glow;
  const bool resize = !surface || width != w || height != h;
  const bool full = resize || dirty || glowing != nextGlow;
  RECT damage = client;
  if (!full) {
    if (selected == nextSelected)
      return;
    RECT cells{};
    for (int i : {selected, nextSelected}) {
      if (i >= 0) {
        const auto &c = panel->candidates[i];
        RECT bounds{(LONG)std::floor((c.left - 1.f) * scale),
                    (LONG)std::floor(2.f * scale),
                    (LONG)std::ceil((c.right + 1.f) * scale),
                    (LONG)std::ceil(40.f * scale)};
        UnionRect(&cells, &cells, &bounds);
      }
    }
    if (!IntersectRect(&damage, &cells, &client))
      return;
  }
  ComPtr<IDCompositionSurface> replacement;
  auto *s = surface.Get();
  dirty = true;
  auto retryOnDeviceLoss = [&]() -> bool {
    if (!recovered && panel->recoverGraphics() && visual != nullptr) {
      recovered = true;
      return true;
    }
    return false;
  };
  if (resize) {
    if (FAILED(panel->compositor->CreateSurface(
            w, h, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ALPHA_MODE_PREMULTIPLIED,
            &replacement))) {
      if (retryOnDeviceLoss())
        continue;
      return;
    }
    s = replacement.Get();
  }
  POINT origin;
  ComPtr<IDXGISurface> dx;
  if (FAILED(
          s->BeginDraw(full ? nullptr : &damage, IID_PPV_ARGS(&dx), &origin))) {
    if (retryOnDeviceLoss())
      continue;
    return;
  }
  ComPtr<ID2D1RenderTarget> rt;
  auto props = D2D1::RenderTargetProperties(
      D2D1_RENDER_TARGET_TYPE_DEFAULT,
      D2D1::PixelFormat(DXGI_FORMAT_B8G8R8A8_UNORM,
                        D2D1_ALPHA_MODE_PREMULTIPLIED),
      (float)panel->dpi, (float)panel->dpi);
  HRESULT painted =
      panel->d2d->CreateDxgiSurfaceRenderTarget(dx.Get(), props, &rt);
  if (SUCCEEDED(painted)) {
    rt->BeginDraw();
    rt->SetTransform(D2D1::Matrix3x2F::Translation(
        (origin.x - damage.left) / scale, (origin.y - damage.top) / scale));
    rt->PushAxisAlignedClip(D2D1::RectF(damage.left / scale, damage.top / scale,
                                        damage.right / scale,
                                        damage.bottom / scale),
                            D2D1_ANTIALIAS_MODE_ALIASED);
    rt->Clear(D2D1::ColorF(0, 0.f));
    ComPtr<ID2D1SolidColorBrush> b;
    painted = rt->CreateSolidColorBrush(D2D1::ColorF(1, 1, 1), &b);
    if (SUCCEEDED(painted)) {
      auto color = [&](RgbaColor c, float alphaScale = 1.f) {
        b->SetColor(D2D1::ColorF(c.r / 255.f, c.g / 255.f, c.b / 255.f,
                                 c.alpha * alphaScale));
      };
      float rad =
          status ? (std::min)((float)radius::kDesktopStatusCapsulePx, dipHeight / 2.f)
                 : (float)radius::kDesktopCandidateWindowPx;
      auto rr = D2D1::RoundedRect(D2D1::RectF(1, 1, dipWidth - 1, dipHeight - 1),
                                  rad, rad);
      color(status ? pal.surface_panel : pal.surface_popover);
      rt->FillRoundedRectangle(rr, b.Get());
      color(pal.border_specular);
      rt->DrawRoundedRectangle(rr, b.Get(), 1.f);

      if (status) {
        // Vitral V mark
        color(pal.accent_text);
        rt->DrawLine({13.f, 12.f}, {20.5f, 27.f}, b.Get(), 2.2f);
        rt->DrawLine({20.5f, 27.f}, {28.f, 12.f}, b.Get(), 2.2f);
        rt->DrawLine({13.f, 12.f}, {20.5f, 17.5f}, b.Get(), 1.4f);
        rt->DrawLine({28.f, 12.f}, {20.5f, 17.5f}, b.Get(), 1.4f);

        // Language pill badge
        auto pill = D2D1::RoundedRect(D2D1::RectF(38.f, 7.f, 76.f, 33.f), 10.f, 10.f);
        color(panel->english ? pal.surface_key_primary : pal.accent_soft);
        rt->FillRoundedRectangle(pill, b.Get());
        color(panel->english ? pal.text_primary : pal.accent_text);
        const std::wstring langText = panel->english ? L"En" : L"中";
        rt->DrawText(langText.data(), (UINT32)langText.size(),
                     panel->smallFont ? panel->smallFont.Get() : panel->font.Get(),
                     D2D1::RectF(38.f, 7.f, 76.f, 33.f), b.Get(),
                     D2D1_DRAW_TEXT_OPTIONS_CLIP);
      } else {
        for (int i = 0; i < panel->count; i++) {
          const auto &candidate = panel->candidates[i];
          const float left = candidate.left, right = candidate.right;
          if ((right + 1.f) * scale <= damage.left || (left - 1.f) * scale >= damage.right)
            continue;
          const bool activeItem = i == nextSelected;
          if (activeItem) {
            auto pill = D2D1::RoundedRect(
                D2D1::RectF(left, 5.f, right, 37.f),
                (float)radius::kCandidateActivePillPx,
                (float)radius::kCandidateActivePillPx);
            color(pal.accent_soft);
            rt->FillRoundedRectangle(pill, b.Get());
            color(pal.accent_primary, 0.38f);
            rt->DrawRoundedRectangle(pill, b.Get(), 1.f);
          }
          color(activeItem ? pal.accent_text : pal.text_primary);
          if (candidate.layout) {
            rt->DrawTextLayout({left, 2.f}, candidate.layout.Get(), b.Get(),
                               D2D1_DRAW_TEXT_OPTIONS_CLIP);
          } else {
            rt->DrawText(candidate.label.data(), (UINT32)candidate.label.size(),
                         panel->font.Get(), D2D1::RectF(left, 2.f, right, 40.f),
                         b.Get(), D2D1_DRAW_TEXT_OPTIONS_CLIP);
          }
        }
        if (panel->ink) {
          // Recessed handwriting surface + subtle guide lines
          auto padRect =
              D2D1::RoundedRect(D2D1::RectF(10.f, 44.f, dipWidth - 10.f, dipHeight - 10.f),
                                12.f, 12.f);
          color(pal.surface_key_function);
          rt->FillRoundedRectangle(padRect, b.Get());
          color(pal.border_specular, 0.7f);
          rt->DrawRoundedRectangle(padRect, b.Get(), 1.f);

          const float midX = dipWidth * 0.5f;
          const float midY = 44.f + (dipHeight - 54.f) * 0.5f;
          color(pal.text_secondary, 0.22f);
          rt->DrawLine({20.f, midY}, {dipWidth - 20.f, midY}, b.Get(), 1.f);
          rt->DrawLine({midX, 54.f}, {midX, dipHeight - 20.f}, b.Get(), 1.f);

          color(pal.ink_stroke_primary);
          auto &p = panel->points;
          for (size_t i = 3; i + 2 < p.size(); i += 3)
            rt->DrawLine({p[i - 3] * dipWidth, 44.f + p[i - 2] * (dipHeight - 54.f)},
                         {p[i] * dipWidth, 44.f + p[i + 1] * (dipHeight - 54.f)}, b.Get(),
                         (float)handwriting::kStrokeWidthPx);
        }
        if (nextGlow) {
          color(pal.retroactive_fix_glow);
          rt->DrawLine({12.f, 40.f}, {dipWidth - 12.f, 40.f}, b.Get(), 2.2f);
        }
      }
    }
    rt->PopAxisAlignedClip();
    const HRESULT drawn = rt->EndDraw();
    if (SUCCEEDED(painted))
      painted = drawn;
  }
  rt.Reset();
  dx.Reset();
  const HRESULT ended = s->EndDraw();
  if (FAILED(painted) || FAILED(ended) ||
      (resize && FAILED(visual->SetContent(s))) ||
      FAILED(panel->compositor->Commit())) {
    if (retryOnDeviceLoss())
      continue;
    return;
  }
  if (resize) {
    surface.Swap(replacement);
    width = w;
    height = h;
  }
  selected = nextSelected;
  glowing = nextGlow;
  dirty = false;
  if (recovered) {
    Surface &other = status ? panel->choices : panel->status;
    if (other.wnd && IsWindowVisible(other.wnd))
      other.redraw();
  }
  return;
  }
}
void Surface::show(int x, int y, int w, int h) {
  if (!wnd)
    return;
  RECT old{};
  GetWindowRect(wnd, &old);
  const bool visible = IsWindowVisible(wnd);
  const bool resize = old.right - old.left != w || old.bottom - old.top != h;
  if (!visible || old.left != x || old.top != y || resize) {
    SetWindowPos(wnd, HWND_TOPMOST, x, y, w, h,
                 SWP_NOACTIVATE | SWP_SHOWWINDOW);
  }
  if (!visible || resize || dirty)
    redraw();
}
void Panel::place() {
  ScopedPanelDpi dpiContext;
  auto fg = GetForegroundWindow();
  const auto foregroundMonitor = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
  MONITORINFO mi{sizeof(mi)};
  if (!active || !fg || !GetMonitorInfoW(foregroundMonitor, &mi)) {
    ShowWindow(status.wnd, SW_HIDE);
    ShowWindow(choices.wnd, SW_HIDE);
    return;
  }
  const bool fullscreen = coversMonitor(fg, mi.rcMonitor);
  const bool hasCaret = caret.left || caret.top || caret.right || caret.bottom;
  auto monitor = foregroundMonitor;
  if (hasCaret) {
    const POINT anchor{caret.left, caret.bottom};
    const auto caretMonitor = MonitorFromPoint(anchor, MONITOR_DEFAULTTONEAREST);
    if (caretMonitor != monitor && GetMonitorInfoW(caretMonitor, &mi))
      monitor = caretMonitor;
  }
  if (!status.wnd || MonitorFromWindow(status.wnd, MONITOR_DEFAULTTONULL) != monitor) {
    setDpi(monitorDpi(monitor, fg));
  } else {
    const UINT measured = dpiContext.perMonitor() ? windowDpi(status.wnd) : 0;
    setDpi(measured ? measured : monitorDpi(monitor, fg));
  }
  const auto a = mi.rcWork;
  POINT mouse{};
  GetCursorPos(&mouse);
  bool was_idle = idle;
  idle = GetTickCount64() - last >= motion::kDesktopIdleFadeDelayMs;
  if (idle != was_idle)
    status.redraw();
  const auto placeStatus = [&] {
    const int sw = px(88);
    const int margin = px(14);
    const bool edge = idle && !(mouse.x >= a.right - sw - margin && mouse.y >= a.bottom - px(65));
    status.show(edge ? a.right - sw / 2 : a.right - sw - margin, a.bottom - px(54),
                sw, px(40));
  };
  if (fullscreen)
    ShowWindow(status.wnd, SW_HIDE);
  else
    placeStatus();
  // The panel HWND reports effective monitor DPI even in a DPI-unaware TSF host.
  if (const UINT measured = dpiContext.perMonitor() && status.wnd &&
                                    MonitorFromWindow(status.wnd, MONITOR_DEFAULTTONULL) == monitor
                                ? windowDpi(status.wnd)
                                : 0;
      measured && measured != dpi) {
    setDpi(measured);
    if (!fullscreen)
      placeStatus();
  }
  if (!ink && !count) {
    ShowWindow(choices.wnd, SW_HIDE);
    return;
  }
  int w = px(ink ? (std::max)(360, totalChoiceWidth) : totalChoiceWidth);
  int h = px(ink ? 216 : 44);
  int x = hasCaret ? caret.left : a.right - w - px(14);
  int y = hasCaret ? caret.bottom + px(4) : a.bottom - h - px(66);
  if (y + h > a.bottom)
    y = caret.top - h - px(4);
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
  case WM_THEMECHANGED:
  case WM_SETTINGCHANGE:
    if (s->status)
      p.refreshTheme();
    break;
  case WM_TIMER:
    if (wp == 2) {
      const auto now = GetTickCount64();
      if (now < p.glow) {
        SetTimer(win, 2, (UINT)(p.glow - now), nullptr);
      } else {
        KillTimer(win, 2);
        s->redraw();
      }
    } else if (p.active) {
      p.refreshTheme();
      p.place();
    }
    return 0;
  case WM_RBUTTONUP: {
    auto menu = CreatePopupMenu();
    AppendMenuW(menu, MF_STRING, 1, L"单字查音");
    AppendMenuW(menu, MF_STRING, 2, L"连续手写");
    AppendMenuW(menu, MF_STRING, 3, L"确认手写上屏");
    AppendMenuW(menu, MF_STRING, 4, L"关闭手写板");
    AppendMenuW(menu, MF_SEPARATOR, 0, nullptr);
    AppendMenuW(menu, MF_STRING, 5, L"打开 Vitral 管理中心");
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
  case WM_MOUSEMOVE:
    if (!s->status && !p.ink && y <= p.px(44)) {
      int idx = p.hitChoice(x / p.scale);
      if (idx >= 0 && p.selected != idx)
        p.send(10, idx);
    }
    break;
  case WM_LBUTTONDOWN: {
    p.last = GetTickCount64();
    if (s->status) {
      if (x < p.px(36))
        PostMessageW(win, WM_RBUTTONUP, 0, lp);
      else
        p.send(1);
      return 0;
    }
    if (y <= p.px(44)) {
      int idx = p.hitChoice(x / p.scale);
      if (idx >= 0)
        p.send(0, idx);
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
                     std::clamp(float(y - p.px(44)) / (std::max)(1L, r.bottom - p.px(54)),
                                0.f, 1.f),
                     float(GetTickCount64() % 600000)});
    s->invalidate();
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
extern "C" void vitral_physical_caret(HWND hwnd, RECT *caret) {
  using ToPhysical = BOOL(WINAPI *)(HWND, LPPOINT);
  static auto convert = reinterpret_cast<ToPhysical>(GetProcAddress(
      GetModuleHandleW(L"user32.dll"), "LogicalToPhysicalPointForPerMonitorDPI"));
  if (!convert || !hwnd ||
      !(caret->left || caret->top || caret->right || caret->bottom))
    return;
  POINT start{caret->left, caret->top}, end{caret->right, caret->bottom};
  if (convert(hwnd, &start) && convert(hwnd, &end))
    *caret = RECT{start.x, start.y, end.x, end.y};
}
extern "C" HRESULT vitral_register(bool install) {
  HRESULT init = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
  if (FAILED(init) && init != RPC_E_CHANGED_MODE)
    return init;
  HRESULT hr = reg(install);
  if (SUCCEEDED(init))
    CoUninitialize();
  return hr;
}
extern "C" HICON vitral_mode_icon(bool english) {
  const int size = (std::max)(16, GetSystemMetrics(SM_CXSMICON));
  BITMAPINFO info{};
  info.bmiHeader.biSize = sizeof(BITMAPINFOHEADER);
  info.bmiHeader.biWidth = size;
  info.bmiHeader.biHeight = -size;
  info.bmiHeader.biPlanes = 1;
  info.bmiHeader.biBitCount = 32;
  HDC dc = CreateCompatibleDC(nullptr);
  if (!dc) return nullptr;
  uint32_t *pixels = nullptr;
  HBITMAP color = CreateDIBSection(dc, &info, DIB_RGB_COLORS,
                                  reinterpret_cast<void **>(&pixels), nullptr, 0);
  if (!color) { DeleteDC(dc); return nullptr; }
  std::fill(pixels, pixels + size * size, 0);
  HGDIOBJ oldBitmap = SelectObject(dc, color);
  HFONT font = CreateFontW(-size, 0, 0, 0, FW_NORMAL, FALSE, FALSE, FALSE,
                          DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
                          ANTIALIASED_QUALITY, DEFAULT_PITCH, L"Microsoft YaHei UI");
  HGDIOBJ oldFont = font ? SelectObject(dc, font) : nullptr;
  SetBkMode(dc, TRANSPARENT);
  SetTextColor(dc, RGB(255, 255, 255));
  RECT rect{0, 0, size, size};
  DrawTextW(dc, english ? L"英" : L"中", 1, &rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
  GdiFlush();
  for (int i = 0; i < size * size; ++i) {
    const uint32_t alpha = (std::max)({pixels[i] & 255u, (pixels[i] >> 8) & 255u,
                                     (pixels[i] >> 16) & 255u});
    // TEXTCOLORICON maps black glyph pixels to the current system text color.
    pixels[i] = alpha << 24;
  }
  if (oldFont) SelectObject(dc, oldFont);
  if (font) DeleteObject(font);
  SelectObject(dc, oldBitmap);
  std::vector<BYTE> maskBits(((size + 15) / 16) * 2 * size, 0);
  HBITMAP mask = CreateBitmap(size, size, 1, 1, maskBits.data());
  ICONINFO iconInfo{TRUE, 0, 0, mask, color};
  HICON icon = mask ? CreateIconIndirect(&iconInfo) : nullptr;
  if (mask) DeleteObject(mask);
  DeleteObject(color);
  DeleteDC(dc);
  return icon;
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
  ScopedPanelDpi dpiContext;
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
    p->bindSurface(*s);
  }
  if (p->status.wnd) {
    SetTimer(p->status.wnd, 1, 750, nullptr);
  }
  return p;
}
extern "C" void vitral_panel_close(Panel *p) {
  if (!p)
    return;
  if (p->status.wnd) {
    DestroyWindow(p->status.wnd);
  }
  if (p->choices.wnd)
    DestroyWindow(p->choices.wnd);
  delete p;
  UnregisterClassW(L"Vitral.Tsf.Panel", module());
}
extern "C" void vitral_panel_update(Panel *p, bool active, bool english,
                                    int mode, PanelText preedit, uint64_t page,
                                    const RECT *caret, const PanelText *words,
                                    int count, int selected, bool glow,
                                    bool activity) {
  if (!p)
    return;
  if (active && !p->active)
    p->refreshTheme();
  if (p->english != english)
    p->status.invalidate();
  const auto preeditText = preedit.view();
  const bool labelsChanged = p->updateChoices(words, std::clamp(count, 0, 5));
  if (labelsChanged || p->preedit != preeditText || p->page != page ||
      p->mode != mode) {
    p->preedit = preeditText;
    p->page = page;
    p->choices.invalidate();
  } else if (!p->ink && p->selected != selected) {
    p->choices.redraw();
  }
  p->active = active;
  p->english = english;
  p->ink = mode != 0;
  p->mode = mode;
  p->selected = selected;
  if (caret)
    p->caret = *caret;
  if (glow && p->choices.wnd) {
    p->glow = GetTickCount64() + handwriting::kRetroactiveHighlightDurationMs;
    SetTimer(p->choices.wnd, 2, handwriting::kRetroactiveHighlightDurationMs,
             nullptr);
    p->choices.redraw();
  }
  if (activity)
    p->last = GetTickCount64();
  p->place();
}
