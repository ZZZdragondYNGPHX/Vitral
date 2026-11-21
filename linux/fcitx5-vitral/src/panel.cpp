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
#include <QElapsedTimer>
#include <QGraphicsOpacityEffect>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QMenu>
#include <QMouseEvent>
#include <QPainter>
#include <QPointer>
#include <QProcess>
#include <QScreen>
#include <QSettings>
#include <QTimer>
#include <QToolButton>
#include <QWindow>

using namespace vitral::tokens;
using Reply = std::function<void(QJsonObject)>;
class Panel;
class Ink : public QWidget {
public:
  explicit Ink(QWidget *parent) : QWidget(parent) {
    setFixedSize(280, 150);
    setAccessibleName("手写区");
    clock_.start();
  }
  std::function<void(QJsonArray)> send;
  void clear() {
    strokes_.clear();
    points_ = {};
    update();
  }

protected:
  void mousePressEvent(QMouseEvent *e) override {
    strokes_.append(QPolygonF{});
    points_ = {};
    point(e);
  }
  void mouseMoveEvent(QMouseEvent *e) override {
    if (e->buttons() & Qt::LeftButton)
      point(e);
  }
  void mouseReleaseEvent(QMouseEvent *e) override {
    point(e);
    if (send && !points_.isEmpty())
      send(points_);
  }
  void paintEvent(QPaintEvent *) override {
    QPainter p(this);
    p.setRenderHint(QPainter::Antialiasing);
    p.setPen(QPen(QColor("#7dd3fc"), handwriting::kStrokeWidthPx, Qt::SolidLine,
                  Qt::RoundCap, Qt::RoundJoin));
    for (const auto &s : strokes_)
      p.drawPolyline(s);
  }

private:
  void point(QMouseEvent *e) {
    if (strokes_.isEmpty())
      return;
    auto pos = e->position();
    strokes_.last().append(pos);
    points_.append(QJsonObject{{"x", pos.x() / width()},
                               {"y", pos.y() / height()},
                               {"timestamp_ms", clock_.elapsed()}});
    update();
  }
  QList<QPolygonF> strokes_;
  QElapsedTimer clock_;
  QJsonArray points_;
};

class Panel : public QWidget {
public:
  Panel()
      : bus_("dev.vitral.IME", "/dev/vitral/IME", "dev.vitral.IME",
             QDBusConnection::sessionBus()) {
    setWindowFlags(Qt::Tool | Qt::FramelessWindowHint |
                   Qt::WindowStaysOnTopHint | Qt::WindowDoesNotAcceptFocus);
    setAttribute(Qt::WA_ShowWithoutActivating);
    setAttribute(Qt::WA_TranslucentBackground);
    auto *layout = new QVBoxLayout(this);
    layout->setContentsMargins(9, 6, 9, 6);
    layout->setSizeConstraint(QLayout::SetFixedSize);
    tools_ = new QHBoxLayout;
    auto *tools = tools_;
    tools->setSpacing(3);
    layout->addLayout(tools);
    makeButton(tools, "Vitral", "vitral-logo", [this] {
      expanded_ = !expanded_;
      if (!expanded_)
        editing_ = false;
      if (!expanded_ && board_->isVisible()) {
        board_->hide();
        ++generation_;
        request("set-preedit", {{"text", ""}});
      }
      refreshToolbar();
    });
    lang_ = makeButton(tools, "切换中英文", "lang-toggle",
                       [this] { request("toggle-language"); });
    extras_ = {
        makeButton(tools, "快捷粘贴", "paste",
                   [this] {
                     request("consume-quick-paste", {}, [this](auto r) {
                       commit(r["consumed"].toObject()["content"].toString());
                     });
                   }),
        makeButton(tools, "手写查音", "pen", [this] { handwriting(false); }),
        makeButton(tools, "连续手写", "draw", [this] { handwriting(true); }),
        makeButton(tools, "翻译选中文本", "translate",
                   [this] { request("translate-selection"); }),
        makeButton(tools, "润色选中文本", "polish",
                   [this] { request("polish-selection"); }),
        makeButton(
            tools, "录音笔", "mic",
            [] { QProcess::startDetached("vitral-desktop", {"--recorder"}); }),
        makeButton(tools, "管理中心", "settings",
                   [] { QProcess::startDetached("vitral-desktop", {}); })};
    edit_ = makeButton(tools, "编排状态栏", "plus", [this] {
      editing_ = true;
      refreshToolbar();
    });
    done_ = makeButton(tools, "完成编排", "ok", [this] {
      editing_ = false;
      refreshToolbar();
    });
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
    quickPaste_ = makeButton(tools, "粘贴最近复制的内容", "paste", [this] {
      request("consume-quick-paste", {}, [this](auto r) {
        commit(r["consumed"].toObject()["content"].toString());
        quickPaste_->hide();
      });
    });
    quickPaste_->hide();
    board_ = new QWidget(this);
    auto *body = new QVBoxLayout(board_);
    ink_ = new Ink(board_);
    body->addWidget(ink_);
    choices_ = new QHBoxLayout;
    body->addLayout(choices_);
    auto *controls = new QHBoxLayout;
    body->addLayout(controls);
    makeButton(controls, "清空", "clear", [this] { handwriting(continuous_); });
    makeButton(controls, "确认", "ok", [this] {
      request("hw-confirm", {}, [this](auto r) {
        commit(r["commit_text"].toString());
        handwriting(continuous_);
      });
    });
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
    setStyleSheet("QToolButton{background:transparent;border:0;color:#f4f8ff;"
                  "padding:4px}QToolButton:hover{background:#305068;border-"
                  "radius:8px}QToolTip{color:#fff;background:#26323f}");
    refreshToolbar();
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
      layer_->setExclusiveZone(0);
    }
    KWindowEffects::enableBlurBehind(windowHandle(), true);
    idle_.start();
    connect(&timer_, &QTimer::timeout, this, [this] { poll(); });
    timer_.start(500);
    auto *cloudTimer = new QTimer(this);
    connect(cloudTimer, &QTimer::timeout, this, [this] {
      if (!cloudWatcher_ || cloudWatcher_->isFinished()) {
        if (cloudWatcher_)
          cloudWatcher_->deleteLater();
        cloudWatcher_ = new QDBusPendingCallWatcher(
            bus_.asyncCall("Command", "poll-services", "{}"), this);
      }
    });
    cloudTimer->start(3000);
    connect(QApplication::clipboard(), &QClipboard::dataChanged, this, [this] {
      auto text = QApplication::clipboard()->text();
      if (!text.isEmpty())
        request("clipboard-push", {{"payload", text}});
    });
    poll();
  }

protected:
  void enterEvent(QEnterEvent *) override {
    idle_.restart();
    faded_ = false;
    reposition();
    update();
  }
  void paintEvent(QPaintEvent *) override {
    QPainter p(this);
    p.setRenderHint(QPainter::Antialiasing);
    p.setPen(QColor(255, 255, 255, 42));
    p.setBrush(QColor(22, 26, 38, 225));
    auto r = board_->isVisible()
                 ? radius::kPopoverPanelPx
                 : std::min(height() / 2, radius::kDesktopStatusCapsulePx);
    p.drawRoundedRect(rect().adjusted(1, 1, -1, -1), r, r);
  }

private:
  QToolButton *makeButton(QHBoxLayout *layout, QString label, QString kind,
                          std::function<void()> click) {
    auto *b = new QToolButton(this);
    b->setObjectName(kind);
    b->setAccessibleName(label);
    b->setToolTip(label);
    b->setFocusPolicy(Qt::NoFocus);
    if (kind == "lang-toggle")
      b->setText("中");
    else {
      QPixmap pix(28, 28);
      pix.fill(Qt::transparent);
      QPainter p(&pix);
      p.setRenderHint(QPainter::Antialiasing);
      p.setPen(QPen(QColor("#bdeaff"), typography::kToolbarIconStrokeWidthPx,
                    Qt::SolidLine, Qt::RoundCap, Qt::RoundJoin));
      if (kind == "vitral-logo")
        p.drawPolygon(QPolygonF{{3, 5}, {14, 24}, {25, 5}, {14, 12}});
      if (kind == "paste") {
        p.drawRoundedRect(6, 6, 16, 19, 3, 3);
        p.drawRoundedRect(10, 3, 8, 5, 2, 2);
      }
      if (kind == "pen" || kind == "draw") {
        p.drawPolygon(QPolygonF{{4, 23}, {7, 16}, {19, 4}, {24, 9}, {12, 21}});
        if (kind == "draw")
          p.drawLine(15, 24, 24, 24);
      }
      if (kind == "translate") {
        p.drawLine(3, 6, 16, 6);
        p.drawLine(9, 3, 9, 6);
        p.drawLine(5, 9, 14, 18);
        p.drawLine(14, 6, 4, 19);
        p.drawPolyline(QPolygonF{{15, 23}, {20, 11}, {25, 23}});
        p.drawLine(17, 19, 23, 19);
      }
      if (kind == "polish") {
        p.drawPolyline(
            QPolygonF{{4, 24}, {7, 17}, {18, 6}, {22, 10}, {11, 21}, {4, 24}});
        p.drawLine(22, 2, 22, 6);
        p.drawLine(20, 4, 24, 4);
      }
      if (kind == "mic") {
        p.drawRoundedRect(10, 3, 8, 14, 4, 4);
        p.drawArc(6, 7, 16, 15, 180 * 16, 180 * 16);
        p.drawLine(14, 22, 14, 25);
      }
      if (kind == "settings") {
        p.drawEllipse(5, 5, 18, 18);
        p.drawEllipse(11, 11, 6, 6);
      }
      if (kind == "clear") {
        p.drawLine(7, 7, 21, 21);
        p.drawLine(7, 21, 21, 7);
      }
      if (kind == "ok")
        p.drawPolyline(QPolygonF{{4, 13}, {11, 20}, {24, 6}});
      if (kind == "plus" || kind == "minus")
        p.drawLine(6, 14, 22, 14);
      if (kind == "plus")
        p.drawLine(14, 6, 14, 22);
      b->setIcon(QIcon(pix));
      b->setIconSize(QSize(23, 23));
    }
    connect(b, &QToolButton::clicked, this,
            [this, b, click = std::move(click)] {
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
  void refreshToolbar() {
    int index = 2;
    for (auto *button : pinned_) {
      tools_->removeWidget(button);
      tools_->insertWidget(index++, button);
    }
    for (auto *button : extras_) {
      button->setVisible(expanded_ || pinned_.contains(button));
      button->setCheckable(editing_);
      button->setChecked(editing_ && pinned_.contains(button));
      if (editing_) {
        if (!button->property("normalIcon").isValid())
          button->setProperty("normalIcon", button->icon());
        auto pix = button->property("normalIcon").value<QIcon>().pixmap(28, 28);
        QPainter paint(&pix);
        paint.setBrush(QColor("#203140"));
        paint.setPen(Qt::NoPen);
        paint.drawEllipse(14, 14, 14, 14);
        paint.setPen(QPen(Qt::white, 2));
        paint.drawLine(17, 21, 25, 21);
        if (!pinned_.contains(button))
          paint.drawLine(21, 17, 21, 25);
        paint.end();
        button->setIcon(QIcon(pix));
      } else if (button->property("normalIcon").isValid()) {
        button->setIcon(button->property("normalIcon").value<QIcon>());
      }
    }
    edit_->setVisible(expanded_ && !editing_);
    done_->setVisible(expanded_ && editing_);
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
                if (cmd == "panel-state") {
                  polling_ = false;
                  hide();
                } else
                  setToolTip(r.error().message());
                return;
              }
              if (reply && (cmd == "panel-state" || editor == editor_))
                reply(QJsonDocument::fromJson(r.value().toUtf8()).object());
            });
  }
  void poll() {
    if (polling_)
      return;
    polling_ = true;
    request("panel-state", {}, [this](auto state) {
      polling_ = false;
      if (editor_ != state["editor"].toInt()) {
        editor_ = state["editor"].toInt();
        ++generation_;
        board_->hide();
        clearChoices();
      }
      bool fullscreen = QApplication::platformName() == "xcb" &&
                        KWindowInfo(KX11Extras::activeWindow(), NET::WMState)
                            .hasState(NET::FullScreen);
      fullscreen |= windows_ && windows_->activeWindow() &&
                    windows_->activeWindow()->isFullscreen();
      if (!state["active"].toBool() || fullscreen) {
        hide();
        return;
      }
      request("quick-paste-state", {}, [this](auto r) {
        auto text = r["quick_paste"].toObject()["content"].toString();
        quickPaste_->setVisible(!text.isEmpty());
        quickPaste_->setToolTip(text.left(80));
      });
      int tick = state["activity"].toInt();
      if (tick != tick_) {
        tick_ = tick;
        idle_.restart();
      }
      faded_ = !underMouse() && !expanded_ &&
               idle_.elapsed() >= motion::kDesktopIdleFadeDelayMs;
      if (!opacity_) {
        opacity_ = new QGraphicsOpacityEffect(this);
        setGraphicsEffect(opacity_);
      }
      opacity_->setOpacity(faded_ ? motion::kDesktopIdleOpacity : 1.0);
      lang_->setText(state["english"].toBool() ? "En" : "中");
      show();
      reposition();
    });
  }
  void reposition() {
    adjustSize();
    const int edgeHideMargin = -width() / 2;
    int right = faded_ ? edgeHideMargin : 12;
    if (layer_) {
      layer_->setMargins({0, 0, right, 12});
      layer_->setDesiredSize(size());
    } else {
      auto r = screen()->availableGeometry();
      move(r.right() - width() - right, r.bottom() - height() - 12);
    }
  }
  void handwriting(bool continuous) {
    ++generation_;
    continuous_ = continuous;
    ink_->clear();
    clearChoices();
    board_->show();
    request("hw-begin",
            {{"mode", continuous ? "ContinuousLattice" : "PhoneticLookup"}});
    reposition();
  }
  void clearChoices() {
    while (auto *item = choices_->takeAt(0)) {
      item->widget()->deleteLater();
      delete item;
    }
  }
  void commit(const QString &text) {
    if (!text.isEmpty())
      request("commit-text", {{"text", text}});
  }
  QToolButton *option(QString label, std::function<void()> click) {
    auto *b = new QToolButton(board_);
    b->setText(label);
    b->setAccessibleName(label);
    connect(b, &QToolButton::clicked, this,
            [this, b, click = std::move(click)] {
              if (editing_ && extras_.contains(b)) {
                if (!pinned_.removeOne(b))
                  pinned_.append(b);
                saveToolbar();
              } else {
                click();
              }
            });
    choices_->addWidget(b);
    return b;
  }
  void candidates(QJsonObject r) {
    clearChoices();
    if (continuous_) {
      request("set-preedit", {{"text", r["composing_text"]}});
      for (const auto &v : r["slots"].toArray()) {
        auto s = v.toObject();
        auto *slot = option(s["char"].toString(), [this, s] {
          clearChoices();
          int i = 0;
          for (auto c : s["top8"].toArray()) {
            auto text = c.toObject()["char"].toString();
            option(text, [this, s, n = i++] {
              request("hw-override-slot",
                      {{"slot_idx", s["slot_idx"]}, {"candidate_idx", n}},
                      [this](auto r) { candidates(r); });
            });
          }
          reposition();
        });
        if (s["changed_retroactively"].toBool()) {
          slot->setStyleSheet("border-bottom:2px solid #38bdf8;");
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
        option(ch + "\n" + c["toned_pinyin"].toString() + " " +
                   c["xiaohe_code"].toString(),
               [this, ch] { commit(ch); });
        if (++n == 5)
          break;
      }
    }
    reposition();
  }
  QSettings preferences_{"Vitral", "Panel"};
  QPointer<KWayland::Client::PlasmaWindowManagement> windows_;
  QHBoxLayout *tools_ = nullptr;
  QGraphicsOpacityEffect *opacity_ = nullptr;
  QToolButton *quickPaste_ = nullptr;
  QList<QToolButton *> pinned_;
  QDBusInterface bus_;
  QToolButton *lang_, *edit_, *done_;
  bool editing_ = false;
  QList<QToolButton *> extras_;
  QWidget *board_;
  Ink *ink_;
  QHBoxLayout *choices_;
  LayerShellQt::Window *layer_ = nullptr;
  QDBusPendingCallWatcher *cloudWatcher_ = nullptr;
  QTimer timer_;
  QElapsedTimer idle_;
  int tick_ = -1, generation_ = 0, editor_ = -1;
  bool expanded_ = false, continuous_ = false, polling_ = false, faded_ = false;
};
int main(int argc, char **argv) {
  QApplication app(argc, argv);
  app.setQuitOnLastWindowClosed(false);
  Panel panel;
  return app.exec();
}
