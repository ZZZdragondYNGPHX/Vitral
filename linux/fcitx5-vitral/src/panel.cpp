#include "clipboard.hpp"
#include "vitral_tokens.hpp"
#include <KWayland/Client/connection_thread.h>
#include <KWayland/Client/plasmawindowmanagement.h>
#include <KWayland/Client/registry.h>
#include <KWindowEffects>
#include <KWindowInfo>
#include <KX11Extras>
#include <LayerShellQt/Window>
#include <QApplication>
#include <QClipboard>
#include <QDBusInterface>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDateTime>
#include <QEasingCurve>
#include <QElapsedTimer>
#include <QFontMetrics>
#include <QGraphicsOpacityEffect>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QMenu>
#include <QMimeData>
#include <QMouseEvent>
#include <QPainter>
#include <QPainterPath>
#include <QPointer>
#include <QProcess>
#include <QPropertyAnimation>
#include <QPushButton>
#include <QScreen>
#include <QScrollArea>
#include <QSettings>
#include <QStyle>
#include <QStyleHints>
#include <QTimer>
#include <QToolButton>
#include <QVBoxLayout>
#include <QWindow>
#include <climits>
#include <cmath>

using namespace vitral::tokens;
using Reply = std::function<void(QJsonObject)>;

namespace {
QColor qcol(const RgbaColor &c, double alphaScale = 1.0) {
  QColor color = QColor::fromRgba(c.argb);
  if (alphaScale < 1.0)
    color.setAlphaF(std::clamp(color.alphaF() * alphaScale, 0.0, 1.0));
  return color;
}
} // namespace

class Ink : public QWidget {
public:
  explicit Ink(QWidget *parent) : QWidget(parent) {
    setFixedSize(316, 166);
    setAccessibleName("手写区");
    setCursor(Qt::CrossCursor);
    clock_.start();
    fadeTimer_.setInterval(24);
    connect(&fadeTimer_, &QTimer::timeout, this, [this] {
      const qint64 now = clock_.elapsed();
      bool anyLeft = false;
      for (int i = strokes_.size() - 1; i >= 0; --i) {
        if (continuous_ && strokes_[i].finishedMs >= 0 &&
            now - strokes_[i].finishedMs >=
                handwriting::kSuperimposedInkFadeOutMs) {
          strokes_.removeAt(i);
        } else if (strokes_[i].finishedMs >= 0) {
          anyLeft = true;
        }
      }
      if (!continuous_ || !anyLeft)
        fadeTimer_.stop();
      update();
    });
  }

  std::function<void(QJsonArray)> send;

  void setPaletteColors(const ThemePalette *palette) {
    colors_ = palette;
    update();
  }

  void setContinuous(bool continuous) {
    continuous_ = continuous;
    clear();
  }

  void clear() {
    strokes_.clear();
    points_ = {};
    fadeTimer_.stop();
    update();
  }

protected:
  void mousePressEvent(QMouseEvent *e) override {
    if (e->button() != Qt::LeftButton)
      return;
    strokes_.append(Stroke{QPolygonF{}, -1});
    points_ = {};
    point(e);
  }

  void mouseMoveEvent(QMouseEvent *e) override {
    if (e->buttons() & Qt::LeftButton)
      point(e);
  }

  void mouseReleaseEvent(QMouseEvent *e) override {
    if (e->button() != Qt::LeftButton)
      return;
    point(e);
    if (!strokes_.isEmpty())
      strokes_.last().finishedMs = clock_.elapsed();
    if (continuous_ && !fadeTimer_.isActive())
      fadeTimer_.start();
    if (send && !points_.isEmpty())
      send(points_);
    update();
  }

  void paintEvent(QPaintEvent *) override {
    QPainter p(this);
    p.setRenderHint(QPainter::Antialiasing);

    // Recessed frosted writing surface
    const QRectF r = QRectF(rect()).adjusted(1, 1, -1, -1);
    p.setPen(QPen(qcol(colors_->border_specular, 0.8), 1.0));
    p.setBrush(qcol(colors_->surface_key_function));
    p.drawRoundedRect(r, 12, 12);

    // Subtle dashed 田字格 guide lines
    QPen guidePen(qcol(colors_->text_secondary, 0.22), 1.0, Qt::DashLine);
    p.setPen(guidePen);
    const qreal cx = r.center().x();
    const qreal cy = r.center().y();
    p.drawLine(QPointF(r.left() + 14, cy), QPointF(r.right() - 14, cy));
    p.drawLine(QPointF(cx, r.top() + 14), QPointF(cx, r.bottom() - 14));

    // Empty-state hint
    if (strokes_.isEmpty()) {
      p.setPen(qcol(colors_->text_secondary, 0.72));
      QFont f = font();
      f.setPointSizeF(9.5);
      p.setFont(f);
      p.drawText(r, Qt::AlignCenter,
                 continuous_ ? "在此原位叠写或横向连写"
                             : "在此手写生僻字（支持慢写补笔）");
      return;
    }

    // Strokes with 420ms fade-out in continuous mode
    const qint64 now = clock_.elapsed();
    const QColor baseInk = qcol(colors_->ink_stroke_primary);
    for (const auto &s : strokes_) {
      double alpha = 1.0;
      if (continuous_ && s.finishedMs >= 0) {
        const double elapsed = static_cast<double>(now - s.finishedMs);
        alpha = std::clamp(
            1.0 - elapsed / handwriting::kSuperimposedInkFadeOutMs, 0.0, 1.0);
      }
      if (alpha <= 0.01)
        continue;
      QColor strokeColor = baseInk;
      strokeColor.setAlphaF(alpha);
      p.setPen(QPen(strokeColor, handwriting::kStrokeWidthPx, Qt::SolidLine,
                    Qt::RoundCap, Qt::RoundJoin));
      if (s.poly.size() == 1)
        p.drawPoint(s.poly.first());
      else
        p.drawPolyline(s.poly);
    }
  }

private:
  struct Stroke {
    QPolygonF poly;
    qint64 finishedMs = -1;
  };

  void point(QMouseEvent *e) {
    if (strokes_.isEmpty())
      return;
    auto pos = e->position();
    strokes_.last().poly.append(pos);
    const auto scale = std::max(width(), height());
    points_.append(QJsonObject{{"x", pos.x() / scale},
                               {"y", pos.y() / scale},
                               {"timestamp_ms", clock_.elapsed()}});
    update();
  }

  QList<Stroke> strokes_;
  QElapsedTimer clock_;
  QTimer fadeTimer_;
  QJsonArray points_;
  const ThemePalette *colors_ = &kDarkTheme;
  bool continuous_ = false;
};

class Panel : public QWidget {
public:
  Panel()
      : bus_("dev.vitral.IME", "/dev/vitral/IME", "dev.vitral.IME",
             QDBusConnection::sessionBus()) {
    const bool isWayland = QApplication::platformName().startsWith("wayland");
    Qt::WindowFlags flags = Qt::FramelessWindowHint | Qt::WindowStaysOnTopHint |
                            Qt::WindowDoesNotAcceptFocus;
    if (!isWayland)
      flags |= Qt::Tool;
    setWindowFlags(flags);
    setAttribute(Qt::WA_ShowWithoutActivating);
    setAttribute(Qt::WA_TranslucentBackground);

    auto *layout = new QVBoxLayout(this);
    layout->setContentsMargins(10, 7, 10, 7);
    layout->setSpacing(6);
    layout->setSizeConstraint(QLayout::SetFixedSize);

    // Top status/toolbar capsule row (never changes height on collapsed hover)
    tools_ = new QHBoxLayout;
    tools_->setSpacing(4);
    tools_->setContentsMargins(0, 0, 0, 0);
    layout->addLayout(tools_);

    logo_ = makeButton(tools_, "展开或收起工具栏", "vitral-logo", [this] {
      expanded_ = !expanded_;
      if (!expanded_) {
        editing_ = false;
        if (board_->isVisible()) {
          board_->hide();
          ++generation_;
          request("set-preedit", {{"text", ""}});
        }
        history_->hide();
      }
      refreshToolbar();
    });
    lang_ = makeButton(tools_, "切换中英文", "lang-toggle",
                       [this] { request("toggle-language"); });

    extras_ = {
        makeButton(tools_, "剪贴板", "paste",
                   [this] {
                     if (history_->isVisible()) {
                       history_->hide();
                       refreshToolbar();
                     } else {
                       clipboard();
                     }
                   }),
        makeButton(tools_, "单字查音", "pen",
                   [this] {
                     if (board_->isVisible() && !continuous_) {
                       closeHandwriting();
                     } else {
                       handwriting(false);
                     }
                   }),
        makeButton(tools_, "连续手写", "draw",
                   [this] {
                     if (board_->isVisible() && continuous_) {
                       closeHandwriting();
                     } else {
                       handwriting(true);
                     }
                   }),
        makeButton(tools_, "翻译选中文本", "translate",
                   [this] { request("translate-selection"); }),
        makeButton(tools_, "润色选中文本", "polish",
                   [this] { request("polish-selection"); }),
        makeButton(
            tools_, "录音笔", "mic",
            [] { QProcess::startDetached("vitral-desktop", {"--recorder"}); }),
        makeButton(tools_, "管理中心", "settings",
                   [] { QProcess::startDetached("vitral-desktop", {}); })};

    edit_ = makeButton(tools_, "自定义状态栏", "plus", [this] {
      editing_ = true;
      refreshToolbar();
    });
    done_ = makeButton(tools_, "完成自定义", "ok", [this] {
      editing_ = false;
      refreshToolbar();
    });

    // Subtle non-jumping caption strip (visible only when expanded)
    captionLabel_ = new QLabel(this);
    captionLabel_->setObjectName("caption");
    captionLabel_->setAlignment(Qt::AlignCenter);
    captionLabel_->hide();
    layout->addWidget(captionLabel_);

    auto saved = preferences_.value("tools/pinned").toStringList();
    for (auto *button : extras_) {
      button->setContextMenuPolicy(Qt::CustomContextMenu);
      connect(button, &QWidget::customContextMenuRequested, this,
              [this, button](QPoint pos) { customizeToolbar(button, pos); });
    }
    for (auto &id : saved)
      for (auto *button : extras_)
        if (id == button->objectName() && !pinned_.contains(button))
          pinned_.append(button);

    // ================= HANDWRITING BOARD =================
    board_ = new QWidget(this);
    auto *body = new QVBoxLayout(board_);
    body->setContentsMargins(2, 4, 2, 2);
    body->setSpacing(8);

    auto *boardHead = new QHBoxLayout;
    boardHead->setSpacing(4);
    lookupModeBtn_ = new QPushButton("单字查音", board_);
    lookupModeBtn_->setFocusPolicy(Qt::NoFocus);
    lookupModeBtn_->setObjectName("segBtn");
    lookupModeBtn_->setCheckable(true);
    connect(lookupModeBtn_, &QPushButton::clicked, this,
            [this] { handwriting(false); });

    contModeBtn_ = new QPushButton("连续手写", board_);
    contModeBtn_->setFocusPolicy(Qt::NoFocus);
    contModeBtn_->setObjectName("segBtn");
    contModeBtn_->setCheckable(true);
    connect(contModeBtn_, &QPushButton::clicked, this,
            [this] { handwriting(true); });

    boardHead->addWidget(lookupModeBtn_);
    boardHead->addWidget(contModeBtn_);
    boardHead->addStretch();
    makeButton(boardHead, "关闭手写", "close", [this] { closeHandwriting(); });
    body->addLayout(boardHead);

    ink_ = new Ink(board_);
    body->addWidget(ink_);

    auto *choiceWrap = new QWidget(board_);
    choices_ = new QHBoxLayout(choiceWrap);
    choices_->setContentsMargins(0, 0, 0, 0);
    choices_->setSpacing(5);
    body->addWidget(choiceWrap);

    auto *controls = new QHBoxLayout;
    controls->setSpacing(6);
    auto *clearBtn = new QPushButton("清空", board_);
    clearBtn->setObjectName("ghostBtn");
    clearBtn->setFocusPolicy(Qt::NoFocus);
    connect(clearBtn, &QPushButton::clicked, this,
            [this] { handwriting(continuous_); });
    controls->addWidget(clearBtn);

    hwHintLabel_ = new QLabel("点击候选直接上屏", board_);
    hwHintLabel_->setObjectName("mutedHint");
    controls->addWidget(hwHintLabel_, 1, Qt::AlignCenter);

    confirmHwBtn_ = new QPushButton("确认上屏", board_);
    confirmHwBtn_->setObjectName("primaryBtn");
    confirmHwBtn_->setFocusPolicy(Qt::NoFocus);
    connect(confirmHwBtn_, &QPushButton::clicked, this, [this] {
      request("hw-confirm", {}, [this, rev = generation_](auto r) {
        if (rev != generation_)
          return;
        commit(r["commit_text"].toString());
        handwriting(continuous_);
      });
    });
    controls->addWidget(confirmHwBtn_);
    body->addLayout(controls);

    // ================= CLIPBOARD POPOVER =================
    history_ = new QWidget(this);
    auto *historyLayout = new QVBoxLayout(history_);
    historyLayout->setContentsMargins(2, 4, 2, 2);
    historyLayout->setSpacing(6);

    auto *historyHead = new QHBoxLayout;
    historyHead->setSpacing(6);
    auto *clipTitle = new QLabel("剪贴板", history_);
    clipTitle->setObjectName("panelTitle");
    clipCountLabel_ = new QLabel("", history_);
    clipCountLabel_->setObjectName("countBadge");
    historyHead->addWidget(clipTitle);
    historyHead->addWidget(clipCountLabel_);
    historyHead->addStretch();
    makeButton(historyHead, "在管理中心打开", "settings", [] {
      QProcess::startDetached("vitral-desktop", {"--clipboard"});
    });
    makeButton(historyHead, "清空未置顶记录", "trash", [this] {
      request("clipboard-clear-unpinned", {},
              [this](auto r) { renderHistory(r); });
    });
    makeButton(historyHead, "关闭剪贴板", "close", [this] {
      history_->hide();
      refreshToolbar();
    });
    historyLayout->addLayout(historyHead);

    auto *scroll = new QScrollArea(history_);
    scroll->setWidgetResizable(true);
    scroll->setFixedSize(348, 236);
    scroll->setFrameShape(QFrame::NoFrame);
    scroll->setHorizontalScrollBarPolicy(Qt::ScrollBarAlwaysOff);
    scroll->setStyleSheet(
        "QScrollArea,QScrollArea>QWidget>QWidget{background:transparent;}");
    auto *entries = new QWidget(scroll);
    historyEntries_ = new QVBoxLayout(entries);
    historyEntries_->setContentsMargins(0, 2, 4, 2);
    historyEntries_->setSpacing(4);
    scroll->setWidget(entries);
    historyLayout->addWidget(scroll);

    layout->addWidget(history_);
    history_->hide();
    layout->addWidget(board_);
    board_->hide();

    ink_->send = [this](auto points) {
      const auto rev = generation_;
      request("hw-push-stroke", {{"points", points}, {"pen_up", true}},
              [this, rev](auto res) {
                if (rev == generation_)
                  candidates(res);
              });
    };

    opacity_ = new QGraphicsOpacityEffect(this);
    opacity_->setOpacity(1.0);
    setGraphicsEffect(opacity_);
    fadeAnim_ = new QPropertyAnimation(opacity_, "opacity", this);
    fadeAnim_->setDuration(motion::kDurationStandardMs);
    fadeAnim_->setEasingCurve(QEasingCurve::OutCubic);

    applyTheme("system");
    winId();
    if (QApplication::platformName().startsWith("wayland")) {
      if (auto *connection =
              KWayland::Client::ConnectionThread::fromApplication(this)) {
        auto *registry = new KWayland::Client::Registry(this);
        connect(registry,
                &KWayland::Client::Registry::plasmaWindowManagementAnnounced,
                this, [this, registry](quint32 name, quint32 version) {
                  windows_ = registry->createPlasmaWindowManagement(
                      name, version, registry);
                });
        registry->create(connection);
        registry->setup();
      }
      layer_ = LayerShellQt::Window::get(windowHandle());
      layer_->setAnchors(
          LayerShellQt::Window::Anchors(LayerShellQt::Window::AnchorBottom) |
          LayerShellQt::Window::AnchorRight);
      layer_->setScope("vitral-status");
      layer_->setLayer(LayerShellQt::Window::LayerTop);
      layer_->setKeyboardInteractivity(
          LayerShellQt::Window::KeyboardInteractivityNone);
      layer_->setActivateOnShow(false);
      layer_->setCloseOnDismissed(false);
      layer_->setWantsToBeOnActiveScreen(true);
      layer_->setExclusiveZone(0);
    }
    KWindowEffects::enableBlurBehind(windowHandle(), true);
    idle_.start();
    connect(&timer_, &QTimer::timeout, this, [this] { poll(); });
    timer_.start(500);
    auto *clipboardTimer = new QTimer(this);
    connect(clipboardTimer, &QTimer::timeout, this, [this] {
      pollClipboardService("clipboard-poll-current", clipboardPolling_);
    });
    clipboardTimer->start(1000);
    auto *cloudTimer = new QTimer(this);
    connect(cloudTimer, &QTimer::timeout, this, [this] {
      pollClipboardService("poll-services", cloudPolling_);
    });
    cloudTimer->start(10000);
    lanClipboardNotification_.setSingleShot(true);
    connect(&lanClipboardNotification_, &QTimer::timeout, this, [this] {
      const auto revision = clipboardRevision_;
      request("quick-paste-state", {}, [this, revision](auto state) {
        applyClipboardState(state, revision);
      });
    });
    bus_.connection().connect("dev.vitral.IME", "/dev/vitral/IME",
                              "dev.vitral.IME", "ClipboardChanged",
                              &lanClipboardNotification_, SLOT(start()));
    if (QApplication::platformName() == "wayland")
      waylandClipboard_ = std::make_unique<WaylandClipboard>(
          [this](const QString &text, const QString &id, qint64 stamp) {
            copied(text, id, stamp);
          });
    connect(QApplication::clipboard(), &QClipboard::dataChanged, this, [this] {
      if (waylandClipboard_ && waylandClipboard_->isActive())
        return;
      auto *mime = QApplication::clipboard()->mimeData();
      if (mime)
        copied(mime->text(),
               QString::fromUtf8(mime->data("application/x-vitral-copy-id")),
               mime->formats().isEmpty() ? 0
                                         : QDateTime::currentMSecsSinceEpoch());
    });
    if (QApplication::platformName() != "wayland") {
      auto *mime = QApplication::clipboard()->mimeData();
      if (mime)
        copied(mime->text(),
               QString::fromUtf8(mime->data("application/x-vitral-copy-id")),
               mime->formats().isEmpty() ? 0
                                         : QDateTime::currentMSecsSinceEpoch());
    }
    poll();
  }

protected:
  bool eventFilter(QObject *object, QEvent *event) override {
    if (auto *button = qobject_cast<QToolButton *>(object)) {
      if (event->type() == QEvent::Enter) {
        if (captionLabel_->isVisible())
          captionLabel_->setText(button->accessibleName());
      } else if (event->type() == QEvent::Leave) {
        updateCaptionDefault();
      }
    }
    return QWidget::eventFilter(object, event);
  }

  void enterEvent(QEnterEvent *) override {
    idle_.restart();
    setFaded(false);
    reposition();
    update();
  }

  void paintEvent(QPaintEvent *) override {
    QPainter p(this);
    p.setRenderHint(QPainter::Antialiasing);
    const QRectF r = QRectF(rect()).adjusted(0.75, 0.75, -0.75, -0.75);
    const qreal rad =
        (board_->isVisible() || history_->isVisible() || expanded_)
            ? static_cast<qreal>(radius::kPopoverPanelPx)
            : std::min(r.height() / 2.0,
                       static_cast<qreal>(radius::kDesktopStatusCapsulePx));

    p.setPen(Qt::NoPen);
    p.setBrush(qcol(colors_->surface_panel));
    p.drawRoundedRect(r, rad, rad);

    // Subtle specular glass rim
    p.setPen(QPen(qcol(colors_->border_specular), 1.0));
    p.setBrush(Qt::NoBrush);
    p.drawRoundedRect(r, rad, rad);

    // Top inner sheen
    QPainterPath clip;
    clip.addRoundedRect(r.adjusted(1, 1, -1, -1), std::max(0.0, rad - 1),
                        std::max(0.0, rad - 1));
    p.setClipPath(clip);
    QColor sheen =
        colors_ == &kDarkTheme ? QColor(255, 255, 255, 26) : QColor(255, 255, 255, 160);
    p.setPen(QPen(sheen, 1.0));
    p.drawLine(QPointF(r.left() + rad * 0.5, r.top() + 1.2),
               QPointF(r.right() - rad * 0.5, r.top() + 1.2));
  }

private:
  void setFaded(bool faded) {
    if (faded_ == faded)
      return;
    faded_ = faded;
    fadeAnim_->stop();
    fadeAnim_->setStartValue(opacity_->opacity());
    fadeAnim_->setEndValue(faded_ ? motion::kDesktopIdleOpacity : 1.0);
    fadeAnim_->start();
  }

  void updateCaptionDefault() {
    if (!captionLabel_->isVisible())
      return;
    if (editing_)
      captionLabel_->setText("点击图标固定或移除 · 点 ✓ 完成");
    else if (board_->isVisible())
      captionLabel_->setText(continuous_ ? "连续手写 · 原位叠写或横向连写"
                                         : "单字查音 · 注全拼与小鹤双拼码");
    else if (history_->isVisible())
      captionLabel_->setText("点击条目直接粘贴 · 星标置顶免淘汰");
    else
      captionLabel_->setText("Vitral 快捷工具 · 右键图标可固定");
  }

  QIcon toolIcon(const QString &kind, const QColor &tint) const {
    QPixmap pix(28, 28);
    pix.fill(Qt::transparent);
    QPainter p(&pix);
    p.setRenderHint(QPainter::Antialiasing);
    p.setPen(QPen(tint, typography::kToolbarIconStrokeWidthPx, Qt::SolidLine,
                  Qt::RoundCap, Qt::RoundJoin));
    p.setBrush(Qt::NoBrush);

    if (kind == "vitral-logo") {
      p.drawPolygon(QPolygonF{{4, 6}, {14, 23}, {24, 6}, {14, 12.5}});
    } else if (kind == "paste") {
      p.drawRoundedRect(QRectF(6.5, 6.5, 15, 17.5), 3, 3);
      p.drawRoundedRect(QRectF(10, 4, 8, 4.5), 1.8, 1.8);
      p.drawLine(QPointF(10, 13.5), QPointF(18, 13.5));
      p.drawLine(QPointF(10, 17.5), QPointF(15.5, 17.5));
    } else if (kind == "pen") {
      p.drawEllipse(QPointF(12, 12), 6.5, 6.5);
      p.drawLine(QPointF(17, 17), QPointF(23, 23));
      p.drawLine(QPointF(9.5, 12), QPointF(14.5, 12));
    } else if (kind == "draw") {
      p.drawPolygon(QPolygonF{{5, 22}, {7.5, 15.5}, {18.5, 4.5}, {23, 9}, {12, 20}});
      p.drawLine(QPointF(15.5, 7.5), QPointF(20, 12));
      p.drawLine(QPointF(15, 23), QPointF(23.5, 23));
    } else if (kind == "translate") {
      p.drawLine(QPointF(4, 7), QPointF(15, 7));
      p.drawLine(QPointF(9.5, 4), QPointF(9.5, 7));
      p.drawLine(QPointF(13, 7), QPointF(5.5, 18.5));
      p.drawLine(QPointF(6.5, 10.5), QPointF(13.5, 17.5));
      p.drawPolyline(QPolygonF{{15.5, 23}, {19.5, 12.5}, {23.5, 23}});
      p.drawLine(QPointF(17, 19.5), QPointF(22, 19.5));
    } else if (kind == "polish") {
      p.drawPolygon(QPolygonF{{12.5, 4.5},
                              {15, 11},
                              {21.5, 13.5},
                              {15, 16},
                              {12.5, 22.5},
                              {10, 16},
                              {3.5, 13.5},
                              {10, 11}});
      p.drawLine(QPointF(22, 4), QPointF(22, 8.5));
      p.drawLine(QPointF(19.8, 6.2), QPointF(24.2, 6.2));
    } else if (kind == "mic") {
      p.drawRoundedRect(QRectF(10.5, 4, 7, 12), 3.5, 3.5);
      p.drawArc(QRectF(7, 8, 14, 12), 180 * 16, 180 * 16);
      p.drawLine(QPointF(14, 20), QPointF(14, 24));
      p.drawLine(QPointF(10.5, 24), QPointF(17.5, 24));
    } else if (kind == "settings") {
      p.drawEllipse(QPointF(14, 14), 3.2, 3.2);
      p.drawEllipse(QPointF(14, 14), 7.2, 7.2);
      for (int i = 0; i < 8; ++i) {
        const double ang = i * M_PI / 4.0;
        p.drawLine(QPointF(14 + 7.2 * std::cos(ang), 14 + 7.2 * std::sin(ang)),
                   QPointF(14 + 10.2 * std::cos(ang), 14 + 10.2 * std::sin(ang)));
      }
    } else if (kind == "trash") {
      p.drawLine(QPointF(6, 8), QPointF(22, 8));
      p.drawLine(QPointF(10.5, 5), QPointF(17.5, 5));
      p.drawRoundedRect(QRectF(8, 8, 12, 15), 2, 2);
      p.drawLine(QPointF(12, 12), QPointF(12, 19));
      p.drawLine(QPointF(16, 12), QPointF(16, 19));
    } else if (kind == "close") {
      p.drawLine(QPointF(8, 8), QPointF(20, 20));
      p.drawLine(QPointF(8, 20), QPointF(20, 8));
    } else if (kind == "ok") {
      p.drawPolyline(QPolygonF{{6, 14.5}, {11.5, 20}, {22, 8}});
    } else if (kind == "plus") {
      p.drawLine(QPointF(7, 14), QPointF(21, 14));
      p.drawLine(QPointF(14, 7), QPointF(14, 21));
    } else if (kind == "pin" || kind == "pin-filled") {
      if (kind == "pin-filled")
        p.setBrush(tint);
      p.drawPolygon(QPolygonF{{10, 5}, {18, 5}, {16.5, 12}, {20, 16}, {8, 16}, {11.5, 12}});
      p.drawLine(QPointF(14, 16), QPointF(14, 23.5));
    }
    return QIcon(pix);
  }

  void applyTheme(const QString &setting) {
    const auto system = QApplication::styleHints()->colorScheme();
    const bool dark =
        setting == "dark" ||
        (setting == "system" &&
         (system == Qt::ColorScheme::Dark ||
          (system == Qt::ColorScheme::Unknown &&
           QApplication::palette().window().color().lightness() < 128)));
    auto *colors = dark ? &kDarkTheme : &kLightTheme;
    if (colors_ == colors && themeApplied_)
      return;
    colors_ = colors;
    themeApplied_ = true;

    ink_->setPaletteColors(colors_);

    const auto cssColor = [](const RgbaColor &c) {
      return QString::fromUtf8(c.css.data());
    };
    const QString textPrimary = cssColor(colors->text_primary);
    const QString textSecondary = cssColor(colors->text_secondary);
    const QString accentPrimary = cssColor(colors->accent_primary);
    const QString accentSoft = cssColor(colors->accent_soft);
    const QString accentText = cssColor(colors->accent_text);
    const QString keyPrimary = cssColor(colors->surface_key_primary);
    const QString keyFunc = cssColor(colors->surface_key_function);
    const QString border = cssColor(colors->border_specular);
    const QString popover = cssColor(colors->surface_popover);

    setStyleSheet(
        QString(
            "QLabel{color:%1;font-size:12px;}"
            "QLabel#caption{color:%2;font-size:11px;padding:1px 4px;}"
            "QLabel#panelTitle{color:%1;font-size:13px;font-weight:600;}"
            "QLabel#mutedHint{color:%2;font-size:11px;}"
            "QLabel#sectionTitle{color:%2;font-size:11px;font-weight:600;padding:4px 4px 2px;}"
            "QLabel#countBadge{color:%5;background:%4;border-radius:8px;padding:1px 7px;font-size:11px;font-weight:600;}"
            "QLabel#codeBadge{color:%5;background:%4;border-radius:5px;padding:1px 6px;font-size:11px;font-weight:600;}"
            "QToolButton{color:%1;background:transparent;border:1px solid transparent;border-radius:9px;padding:4px;}"
            "QToolButton:hover{background:%4;border-color:%8;}"
            "QToolButton[active=\"true\"]{background:%4;color:%5;border-color:%3;}"
            "QToolButton#lang-toggle{min-width:26px;font-weight:700;font-size:12px;padding:3px 7px;border-radius:9px;}"
            "QToolButton#lang-toggle[zh=\"true\"]{background:%4;color:%5;}"
            "QToolButton#lang-toggle[zh=\"false\"]{background:%6;color:%1;}"
            "QToolButton#clipText{text-align:left;padding:6px 8px;border-radius:8px;background:%7;}"
            "QToolButton#clipText:hover{background:%4;color:%5;}"
            "QToolButton#candBtn{padding:5px 9px;border-radius:9px;background:%7;border:1px solid %8;font-size:14px;}"
            "QToolButton#candBtn:hover{background:%4;color:%5;border-color:%3;}"
            "QPushButton#segBtn{color:%2;background:%7;border:1px solid transparent;border-radius:8px;padding:4px 10px;font-size:12px;font-weight:500;}"
            "QPushButton#segBtn:checked{color:%5;background:%4;border-color:%3;font-weight:600;}"
            "QPushButton#ghostBtn{color:%1;background:%6;border:1px solid %8;border-radius:8px;padding:4px 11px;font-size:12px;}"
            "QPushButton#ghostBtn:hover{background:%4;color:%5;}"
            "QPushButton#primaryBtn{color:#ffffff;background:%3;border:1px solid transparent;border-radius:8px;padding:4px 12px;font-size:12px;font-weight:600;}"
            "QPushButton#primaryBtn:hover{background:%5;color:#0b1019;}"
            "QToolTip{color:%1;background:%9;border:1px solid %8;border-radius:6px;padding:4px 7px;}"
            "QScrollBar:vertical{width:5px;background:transparent;}"
            "QScrollBar::handle:vertical{background:%8;border-radius:2px;min-height:20px;}"
            "QScrollBar::add-line:vertical,QScrollBar::sub-line:vertical{height:0;}")
            .arg(textPrimary, textSecondary, accentPrimary, accentSoft,
                 accentText, keyPrimary, keyFunc, border, popover));

    refreshToolbar();
    update();
  }

  QToolButton *makeButton(QHBoxLayout *layout, QString label, QString kind,
                          std::function<void()> click) {
    auto *b = new QToolButton(this);
    b->setObjectName(kind);
    b->setAccessibleName(label);
    b->setToolTip(label);
    b->installEventFilter(this);
    b->setFocusPolicy(Qt::NoFocus);
    b->setCursor(Qt::PointingHandCursor);
    if (kind == "lang-toggle") {
      b->setText("中");
      b->setProperty("zh", true);
    } else {
      b->setIconSize(QSize(20, 20));
    }
    connect(b, &QToolButton::clicked, this,
            [this, b, click = std::move(click)] {
              idle_.restart();
              setFaded(false);
              if (editing_ && extras_.contains(b)) {
                if (!pinned_.removeOne(b))
                  pinned_.append(b);
                saveToolbar();
              } else {
                click();
              }
            });
    layout->addWidget(b);
    return b;
  }

  void refreshButtonIcons() {
    const QColor normalTint = qcol(colors_->text_primary, 0.88);
    const QColor accentTint = qcol(colors_->accent_text);

    auto applyIcon = [&](QToolButton *b, bool active) {
      if (!b || b == lang_)
        return;
      b->setProperty("active", active);
      b->style()->unpolish(b);
      b->style()->polish(b);
      const QColor tint =
          (active || b == logo_) ? accentTint : normalTint;
      QIcon icon = toolIcon(b->objectName(), tint);
      if (editing_ && extras_.contains(b)) {
        auto pix = icon.pixmap(28, 28);
        QPainter paint(&pix);
        paint.setRenderHint(QPainter::Antialiasing);
        const bool isPinned = pinned_.contains(b);
        paint.setBrush(isPinned ? QColor(239, 68, 68) : QColor(16, 185, 129));
        paint.setPen(Qt::NoPen);
        paint.drawEllipse(15, 15, 12, 12);
        paint.setPen(QPen(Qt::white, 1.8, Qt::SolidLine, Qt::RoundCap));
        paint.drawLine(18, 21, 24, 21);
        if (!isPinned)
          paint.drawLine(21, 18, 21, 24);
        paint.end();
        icon = QIcon(pix);
      }
      b->setIcon(icon);
    };

    applyIcon(logo_, expanded_);
    for (auto *b : extras_) {
      bool active = false;
      if (b->objectName() == "paste")
        active = history_->isVisible();
      else if (b->objectName() == "pen")
        active = board_->isVisible() && !continuous_;
      else if (b->objectName() == "draw")
        active = board_->isVisible() && continuous_;
      applyIcon(b, active);
    }
    applyIcon(edit_, false);
    applyIcon(done_, editing_);
    for (auto *b : board_->findChildren<QToolButton *>())
      if (!b->objectName().isEmpty() && b->objectName() != "candBtn")
        applyIcon(b, false);
    for (auto *b : history_->findChildren<QToolButton *>())
      if (!b->objectName().isEmpty() && b->objectName() != "clipText" &&
          b->objectName() != "pinBtn")
        applyIcon(b, false);
  }

  void refreshToolbar() {
    captionLabel_->setVisible(expanded_ || board_->isVisible() ||
                              history_->isVisible());
    updateCaptionDefault();

    int index = 2;
    for (auto *button : pinned_) {
      tools_->removeWidget(button);
      tools_->insertWidget(index++, button);
    }
    for (auto *button : extras_) {
      button->setVisible(expanded_ || pinned_.contains(button));
    }
    edit_->setVisible(expanded_ && !editing_);
    done_->setVisible(expanded_ && editing_);
    refreshButtonIcons();
    reposition();
  }

  void customizeToolbar(QToolButton *button, QPoint pos) {
    QMenu menu;
    bool pinned = pinned_.contains(button);
    auto *toggle =
        menu.addAction(pinned ? tr("从状态栏移除") : tr("固定到状态栏"));
    auto *earlier = menu.addAction(tr("向前移动"));
    earlier->setEnabled(pinned_.indexOf(button) > 0);
    auto *selected = menu.exec(button->mapToGlobal(pos));
    if (selected == toggle) {
      if (pinned)
        pinned_.removeOne(button);
      else
        pinned_.append(button);
    } else if (selected == earlier) {
      int i = pinned_.indexOf(button);
      pinned_.swapItemsAt(i, i - 1);
    } else
      return;
    saveToolbar();
  }

  void saveToolbar() {
    QStringList ids;
    for (auto *b : pinned_)
      ids.append(b->objectName());
    preferences_.setValue("tools/pinned", ids);
    refreshToolbar();
  }

  void request(QString cmd, QJsonObject args = {}, Reply reply = {}) {
    int editor = editor_;
    if (cmd == "commit-text" || cmd == "set-preedit" ||
        cmd == "toggle-language" || cmd == "translate-selection" ||
        cmd == "polish-selection")
      args.insert("editor", editor);
    auto pending = bus_.asyncCall(
        "Command", cmd,
        QString::fromUtf8(QJsonDocument(args).toJson(QJsonDocument::Compact)));
    auto *w = new QDBusPendingCallWatcher(pending, this);
    connect(w, &QDBusPendingCallWatcher::finished, this,
            [this, w, reply, cmd, editor] {
              QDBusPendingReply<QString> r = *w;
              w->deleteLater();
              if (r.isError()) {
                if (cmd == "quick-paste-state")
                  clipboardSyncing_ = false;
                if (cmd == "panel-state") {
                  polling_ = false;
                  hide();
                } else
                  setToolTip(r.error().message());
                return;
              }
              if (reply && (cmd == "panel-state" ||
                            cmd == "quick-paste-state" ||
                            cmd == "clipboard-push" || editor == editor_))
                reply(QJsonDocument::fromJson(r.value().toUtf8()).object());
            });
  }

  QString systemCopyId_;
  quint64 clipboardRevision_ = 0;
  bool clipboardSyncing_ = false;
  bool clipboardPolling_ = false, cloudPolling_ = false;
  qint64 appliedCopyAt_ = 0;
  qint64 localCopyAt_ = 0;
  QString localCopyText_;

  void copied(const QString &text, const QString &id, qint64 stamp) {
    ++clipboardRevision_;
    if (!id.isEmpty()) {
      systemCopyId_ = id;
      return;
    }
    if (text.isEmpty()) {
      if (stamp)
        localCopyAt_ = stamp;
      localCopyText_.clear();
      return;
    }
    if (systemCopyId_.isEmpty() &&
        preferences_.value("clipboard/text").toString() == text) {
      systemCopyId_ = preferences_.value("clipboard/id").toString();
      if (!systemCopyId_.isEmpty()) {
        localCopyAt_ = 0;
        return;
      }
    }
    localCopyAt_ = stamp;
    localCopyText_ = text;
    request("clipboard-push",
            {{"payload",
              QJsonObject{{"content", text}, {"timestamp_ms", stamp}}}});
  }

  void syncClipboard() {
    if (clipboardSyncing_)
      return;
    clipboardSyncing_ = true;
    const auto revision = clipboardRevision_;
    request("quick-paste-state", {}, [this, revision](auto state) {
      clipboardSyncing_ = false;
      applyClipboardState(state, revision);
    });
  }

  void pollClipboardService(const QString &command, bool &polling) {
    if (polling)
      return;
    polling = true;
    const auto revision = clipboardRevision_;
    auto *watcher = new QDBusPendingCallWatcher(
        bus_.asyncCall("Command", command, "{}"), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this,
            [this, watcher, revision, &polling] {
              QDBusPendingReply<QString> reply = *watcher;
              watcher->deleteLater();
              polling = false;
              if (!reply.isError())
                applyClipboardState(
                    QJsonDocument::fromJson(reply.value().toUtf8()).object(),
                    revision);
            });
  }

  void applyClipboardState(const QJsonObject &state, quint64 revision) {
    if (revision != clipboardRevision_)
      return;
    const auto copy = state["current_copy"].toObject();
    const auto id = copy["id"].toString();
    const auto text = copy["content"].toString();
    const auto stamp = copy["timestamp_ms"].toInteger();
    if (id.isEmpty() || id == systemCopyId_ || stamp < localCopyAt_ ||
        stamp < appliedCopyAt_ ||
        (stamp == appliedCopyAt_ && id.toUtf8() < systemCopyId_.toUtf8()))
      return;
    if (stamp == localCopyAt_ && text == localCopyText_) {
      systemCopyId_ = id;
    } else if (waylandClipboard_ && waylandClipboard_->isActive()) {
      if (!waylandClipboard_->setText(text, id))
        return;
      systemCopyId_ = id;
    } else {
      auto *mime = new QMimeData;
      mime->setText(text);
      mime->setData("application/x-vitral-copy-id", id.toUtf8());
      systemCopyId_ = id;
      QApplication::clipboard()->setMimeData(mime);
    }
    appliedCopyAt_ = stamp;
    preferences_.setValue("clipboard/id", id);
    preferences_.setValue("clipboard/text", text);
  }

  int functionSerial_ = 0;
  void poll() {
    syncClipboard();
    if (polling_)
      return;
    polling_ = true;
    request("panel-state", {}, [this](auto state) {
      polling_ = false;
      applyTheme(state["theme"].toString("system"));
      if (editor_ != state["editor"].toInt()) {
        editor_ = state["editor"].toInt();
        ++generation_;
        board_->hide();
        history_->hide();
        clearChoices();
        refreshToolbar();
      }
      bool fullscreen = QApplication::platformName() == "xcb" &&
                        KWindowInfo(KX11Extras::activeWindow(), NET::WMState)
                            .hasState(NET::FullScreen);
      fullscreen |= windows_ && windows_->activeWindow() &&
                    windows_->activeWindow()->isFullscreen();
      const int serial = state["function_serial"].toInt();
      const bool newFunction = serial != functionSerial_ &&
                               state["function_editor"].toInt(-1) == editor_;
      functionSerial_ = serial;
      if (!state["active"].toBool() || fullscreen) {
        hide();
        return;
      }
      if (history_->isVisible()) {
        const auto generation = generation_;
        request("clipboard-state", {}, [this, generation](auto r) {
          if (generation == generation_ && history_->isVisible())
            renderHistory(r);
        });
      }
      int tick = state["activity"].toInt();
      if (tick != tick_) {
        tick_ = tick;
        idle_.restart();
      }
      const bool shouldFade = !underMouse() && !expanded_ &&
                              !board_->isVisible() && !history_->isVisible() &&
                              idle_.elapsed() >= motion::kDesktopIdleFadeDelayMs;
      setFaded(shouldFade);
      if (newFunction) {
        const int function = state["function"].toInt();
        if (function == 1)
          clipboard();
        else if (function == 2 || function == 3)
          handwriting(function == 3);
      }
      const bool eng = state["english"].toBool();
      lang_->setText(eng ? "En" : "中");
      if (lang_->property("zh").toBool() != !eng) {
        lang_->setProperty("zh", !eng);
        lang_->style()->unpolish(lang_);
        lang_->style()->polish(lang_);
      }
      lang_->setVisible(state["english_mode_enabled"].toBool(true));
      reposition();
      if (!isVisible())
        show();
    });
  }

  void reposition() {
    adjustSize();
    const QSize sz = size().expandedTo(sizeHint());
    if (sz != size())
      resize(sz);
    const int edgeHideMargin = -sz.width() / 2;
    const int right = faded_ ? edgeHideMargin : 14;
    if (sz == lastSize_ && right == lastRight_)
      return;
    lastSize_ = sz;
    lastRight_ = right;
    if (layer_) {
      layer_->setMargins({0, 0, right, 14});
      layer_->setDesiredSize(sz);
    } else if (auto *scr = screen()) {
      auto r = scr->availableGeometry();
      move(r.right() - sz.width() - right, r.bottom() - sz.height() - 14);
    }
  }

  void closeHandwriting() {
    ++generation_;
    if (continuous_ && board_->isVisible())
      request("set-preedit", {{"text", ""}});
    board_->hide();
    clearChoices();
    refreshToolbar();
  }

  void clipboard() {
    ++generation_;
    if (continuous_ && board_->isVisible())
      request("set-preedit", {{"text", ""}});
    board_->hide();
    history_->show();
    refreshToolbar();
    request("clipboard-state", {}, [this](auto r) { renderHistory(r); });
  }

  void renderHistory(const QJsonObject &state) {
    if (state == historyState_)
      return;
    historyState_ = state;
    while (auto *item = historyEntries_->takeAt(0)) {
      if (item->widget()) {
        item->widget()->hide();
        item->widget()->deleteLater();
      }
      delete item;
    }
    const auto ring = state["ring"].toArray();
    const int capacity = state["capacity"].toInt(50);
    clipCountLabel_->setText(
        QString("%1 / %2").arg(ring.size()).arg(capacity));

    if (ring.isEmpty()) {
      auto *empty = new QLabel("暂无复制记录", history_);
      empty->setObjectName("mutedHint");
      empty->setAlignment(Qt::AlignCenter);
      empty->setContentsMargins(0, 28, 0, 28);
      historyEntries_->addWidget(empty);
      historyEntries_->addStretch();
      return;
    }

    for (bool pinned : {true, false}) {
      bool addedHeader = false;
      for (const auto &value : ring) {
        auto entry = value.toObject();
        if (entry["pinned"].toBool() != pinned)
          continue;
        if (!addedHeader) {
          addedHeader = true;
          auto *hdr = new QLabel(pinned ? "置顶收藏" : "最近复制", history_);
          hdr->setObjectName("sectionTitle");
          historyEntries_->addWidget(hdr);
        }
        auto *row = new QWidget(history_);
        auto *layout = new QHBoxLayout(row);
        layout->setContentsMargins(0, 0, 0, 0);
        layout->setSpacing(4);

        auto *text = new QToolButton(row);
        text->setObjectName("clipText");
        const auto content = entry["content"].toString();
        const QString preview =
            QFontMetrics(text->font())
                .elidedText(content.simplified(), Qt::ElideRight, 236);
        text->setText(preview);
        text->setAccessibleName(content);
        text->setToolTip(content);
        text->setFocusPolicy(Qt::NoFocus);
        text->setCursor(Qt::PointingHandCursor);
        text->setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Preferred);
        connect(text, &QToolButton::clicked, this, [this, content] {
          commit(content, true);
          history_->hide();
          refreshToolbar();
        });
        layout->addWidget(text, 1);

        const auto shortcutCode = entry["shortcut_code"].toString();
        if (pinned && !shortcutCode.isEmpty()) {
          auto *codeLbl = new QLabel(shortcutCode, row);
          codeLbl->setObjectName("codeBadge");
          layout->addWidget(codeLbl);
        }

        auto *pin = new QToolButton(row);
        pin->setObjectName("pinBtn");
        pin->setIcon(toolIcon(
            pinned ? "pin-filled" : "pin",
            pinned ? qcol(colors_->accent_text)
                   : qcol(colors_->text_secondary)));
        pin->setIconSize(QSize(16, 16));
        pin->setAccessibleName(pinned ? "取消置顶" : "置顶");
        pin->setToolTip(pinned ? "取消置顶" : "置顶收藏");
        pin->setFocusPolicy(Qt::NoFocus);
        pin->setCursor(Qt::PointingHandCursor);
        connect(pin, &QToolButton::clicked, this, [this, entry, pinned] {
          request(pinned ? "clipboard-unpin" : "clipboard-pin",
                  {{"id", entry["id"]}},
                  [this](auto r) { renderHistory(r); });
        });
        layout->addWidget(pin);

        historyEntries_->addWidget(row);
      }
    }
    historyEntries_->addStretch();
  }

  void handwriting(bool continuous) {
    ++generation_;
    if (continuous_ && board_->isVisible())
      request("set-preedit", {{"text", ""}});
    history_->hide();
    continuous_ = continuous;
    lookupModeBtn_->setChecked(!continuous);
    contModeBtn_->setChecked(continuous);
    confirmHwBtn_->setVisible(continuous);
    hwHintLabel_->setText(continuous ? "点击字位改选备选" : "点击候选字直接上屏");
    ink_->setContinuous(continuous);
    clearChoices();
    board_->show();
    refreshToolbar();
    request("hw-begin",
            {{"mode", continuous ? "ContinuousLattice" : "PhoneticLookup"}});
  }

  void clearChoices() {
    while (auto *item = choices_->takeAt(0)) {
      if (item->widget()) {
        item->widget()->hide();
        item->widget()->deleteLater();
      }
      delete item;
    }
  }

  void commit(const QString &text, bool fromClipboard = false) {
    if (!text.isEmpty())
      request("commit-text",
              {{"text", text}, {"clipboard_commit", fromClipboard}});
  }

  QToolButton *option(QString label, std::function<void()> click) {
    auto *b = new QToolButton(board_);
    b->setObjectName("candBtn");
    b->setText(label);
    b->setAccessibleName(label);
    b->setFocusPolicy(Qt::NoFocus);
    b->setCursor(Qt::PointingHandCursor);
    connect(b, &QToolButton::clicked, this, std::move(click));
    choices_->addWidget(b);
    return b;
  }

  void candidates(QJsonObject r) {
    lastHwResult_ = r;
    clearChoices();
    if (continuous_) {
      request("set-preedit", {{"text", r["composing_text"]}});
      for (const auto &v : r["slots"].toArray()) {
        auto s = v.toObject();
        auto *slot = option(s["char"].toString(), [this, s] {
          clearChoices();
          option("←", [this] { candidates(lastHwResult_); });
          int i = 0;
          for (auto c : s["top8"].toArray()) {
            auto text = c.toObject()["char"].toString();
            option(text, [this, s, n = i++] {
              request("hw-override-slot",
                      {{"slot_idx", s["slot_idx"]}, {"candidate_idx", n}},
                      [this, rev = generation_](auto res) {
                        if (rev == generation_)
                          candidates(res);
                      });
            });
          }
          reposition();
        });
        if (s["changed_retroactively"].toBool()) {
          slot->setStyleSheet(
              QString("border:1px solid %1;background:%2;")
                  .arg(QString::fromUtf8(colors_->retroactive_fix_glow.css.data()),
                       QString::fromUtf8(colors_->accent_soft.css.data())));
          QTimer::singleShot(handwriting::kRetroactiveHighlightDurationMs, slot,
                             [slot] { slot->setStyleSheet({}); });
        }
      }
    } else {
      int n = 0;
      for (const auto &v : r["candidates"].toArray()) {
        auto c = v.toObject();
        auto ch = c["ch"].toString();
        if (ch.isEmpty())
          ch = c["char"].toString();
        const auto py = c["toned_pinyin"].toString();
        const auto code = c["xiaohe_code"].toString();
        const QString subtitle =
            code.isEmpty() ? py : QString("%1 · %2").arg(py, code);
        option(ch + "\n" + subtitle, [this, ch] {
          commit(ch);
          handwriting(false);
        });
        if (++n == 5)
          break;
      }
    }
    reposition();
  }

  std::unique_ptr<WaylandClipboard> waylandClipboard_;
  QSettings preferences_{"Vitral", "Panel"};
  QPointer<KWayland::Client::PlasmaWindowManagement> windows_;
  QHBoxLayout *tools_ = nullptr;
  QGraphicsOpacityEffect *opacity_ = nullptr;
  QPropertyAnimation *fadeAnim_ = nullptr;
  QList<QToolButton *> pinned_;
  QDBusInterface bus_;
  QToolButton *logo_ = nullptr, *lang_ = nullptr, *edit_ = nullptr,
              *done_ = nullptr;
  QPushButton *lookupModeBtn_ = nullptr, *contModeBtn_ = nullptr,
              *confirmHwBtn_ = nullptr;
  bool editing_ = false;
  QList<QToolButton *> extras_;
  QWidget *board_ = nullptr, *history_ = nullptr;
  QJsonObject historyState_;
  QVBoxLayout *historyEntries_ = nullptr;
  QLabel *captionLabel_ = nullptr, *hwHintLabel_ = nullptr,
         *clipCountLabel_ = nullptr;
  QJsonObject lastHwResult_;
  const ThemePalette *colors_ = &kDarkTheme;
  bool themeApplied_ = false;
  Ink *ink_ = nullptr;
  QHBoxLayout *choices_ = nullptr;
  LayerShellQt::Window *layer_ = nullptr;
  QTimer timer_, lanClipboardNotification_;
  QElapsedTimer idle_;
  QSize lastSize_;
  int lastRight_ = INT_MIN;
  int tick_ = -1, generation_ = 0, editor_ = -1;
  bool expanded_ = false, continuous_ = false, polling_ = false, faded_ = false;
};

int main(int argc, char **argv) {
  qputenv("QT_WAYLAND_USE_DATA_CONTROL", "1");
  if (qEnvironmentVariableIsEmpty("QT_WAYLAND_SHELL_INTEGRATION") &&
      (!qEnvironmentVariableIsEmpty("WAYLAND_DISPLAY") ||
       qgetenv("XDG_SESSION_TYPE") == "wayland")) {
    qputenv("QT_WAYLAND_SHELL_INTEGRATION", "layer-shell");
  }
  QApplication app(argc, argv);
  app.setQuitOnLastWindowClosed(false);
  if (!QDBusConnection::sessionBus().registerService("dev.vitral.Panel"))
    return 0;
  Panel panel;
  return app.exec();
}
