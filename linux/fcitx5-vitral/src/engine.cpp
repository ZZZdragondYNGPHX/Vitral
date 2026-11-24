#include "control.hpp"
#include <QFile>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QProcess>
#include <QProcessEnvironment>
#include <QSettings>
#include <QStandardPaths>
#include <QStringList>
#include <QSysInfo>
#include <QUuid>
#include <atomic>
#include <csignal>
#include <fcitx-config/configuration.h>
#include <fcitx-config/rawconfig.h>
#include <fcitx-utils/event.h>
#include <fcitx-utils/eventdispatcher.h>
#include <fcitx-utils/log.h>
#include <fcitx-utils/utf8.h>
#include <fcitx/addonfactory.h>
#include <fcitx/addonmanager.h>
#include <fcitx/candidatelist.h>
#include <fcitx/event.h>
#include <fcitx/inputcontext.h>
#include <fcitx/inputmethodengine.h>
#include <fcitx/inputpanel.h>
#include <fcitx/instance.h>
#include <functional>
#include <set>
#include <stdexcept>

#include "vitral_core.h"
#include "vitral_tokens.hpp"

namespace {
using namespace fcitx;
using namespace vitral::tokens;

constexpr bool canReportIdleDeletion(CapabilityFlags capabilities) {
  constexpr auto passwordOrSensitive =
      static_cast<CapabilityFlags::storage_type>(CapabilityFlag::PasswordOrSensitive);
  return (capabilities.toInteger() & passwordOrSensitive) == 0;
}

static_assert(canReportIdleDeletion(CapabilityFlag::NoFlag));
static_assert(!canReportIdleDeletion(CapabilityFlag::Password));
static_assert(!canReportIdleDeletion(CapabilityFlag::Sensitive));

QJsonObject unpack(VitralStringResult result) {
  QJsonObject data;
  if (result.status == VITRAL_STATUS_OK && result.json_utf8) {
    data = QJsonDocument::fromJson(result.json_utf8).object();
  } else {
    FCITX_ERROR() << "Vitral: "
                  << (result.error_message ? result.error_message
                                           : "core operation failed");
  }
  vitral_free_result(result);
  return data;
}

class Choice final : public CandidateWord {
public:
  using Select = std::function<void(InputContext *)>;
  static QString displayText(const QJsonObject &candidate) {
    auto text = candidate["text"].toString();
    if (candidate.contains("quick_paste_id")) {
      text = text.simplified();
      if (text.size() > 8)
        text = text.left(8) + "…";
    }
    return text;
  }
  Choice(const QJsonObject &candidate, Select select)
      : CandidateWord(Text(displayText(candidate).toStdString())),
        select_(std::move(select)) {
    setComment(Text(candidate["comment"].toString().toStdString()));
    setCustomLabel(Text(std::to_string(candidate["index"].toInt() + 1) + " "));
  }
  void select(InputContext *ic) const override {
    auto callback = select_;
    callback(ic);
  }

private:
  Select select_;
};

class PagedCandidateList final : public CommonCandidateList {
public:
  using Page = std::function<void(KeySym)>;
  PagedCandidateList(bool hasPrev, bool hasNext, Page page)
      : hasPrev_(hasPrev), hasNext_(hasNext), page_(std::move(page)) {}
  bool hasPrev() const override { return hasPrev_; }
  bool hasNext() const override { return hasNext_; }
  bool usedNextBefore() const override { return hasPrev_; }
  void prev() override {
    if (hasPrev_) {
      auto callback = page_;
      callback(FcitxKey_Page_Up);
    }
  }
  void next() override {
    if (hasNext_) {
      auto callback = page_;
      callback(FcitxKey_Page_Down);
    }
  }

private:
  bool hasPrev_, hasNext_;
  Page page_;
};

class Engine final : public InputMethodEngineV2 {
public:
  explicit Engine(Instance *instance) : instance_(instance) {
    dispatcher_.attach(&instance_->eventLoop());
    auto configDir =
        QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation);
    QFile file(configDir + "/vitral/settings.json");
    QJsonObject cfg;
    if (file.open(QIODevice::ReadOnly))
      cfg = QJsonDocument::fromJson(file.readAll()).object();
    predictionDelay_ =
        std::clamp(cfg.value("prediction_delay_ms").toInt(380), 150, 2000);
    predictionEnabled_ = cfg.value("prediction_enabled").toBool(true);
    internalEnglish_ = cfg.value("linux_english_mode").toBool(true);
    setPanelTheme(cfg);
    const auto data =
        QStandardPaths::writableLocation(QStandardPaths::GenericDataLocation) +
        "/vitral";
    QSettings identity("Vitral", "Installation");
    if (!identity.contains("id"))
      identity.setValue("id",
                        QUuid::createUuid().toString(QUuid::WithoutBraces));
    cfg.insert("device_id", identity.value("id").toString());
    cfg.insert("device_name", QSysInfo::machineHostName());
    cfg.insert("data_dir", data);
    cfg.insert("background_sync", true);
    const auto assets = QStandardPaths::locate(
        QStandardPaths::GenericDataLocation, "vitral/assets",
        QStandardPaths::LocateDirectory);
    if (!cfg.contains("assets_dir") && !assets.isEmpty())
      cfg.insert("assets_dir", assets);
    const auto encoded = QJsonDocument(cfg).toJson(QJsonDocument::Compact);
    char *error = nullptr;
    if (vitral_init(encoded.constData(), &core_, &error) != VITRAL_STATUS_OK) {
      std::string why = error ? error : "Vitral initialization failed";
      vitral_free_string(error);
      throw std::runtime_error(why);
    }
    shortcuts_ = command("shortcuts-state")["bindings"].toArray();
    control_ = std::make_unique<DesktopControl>(
        core_,
        [this](std::string cmd, std::string args) {
          auto payload =
              QJsonDocument::fromJson(QByteArray::fromStdString(args)).object();
          if (cmd == "configure") {
            dispatcher_.schedule([this, payload] { applyAppearance(payload); });
            return;
          }
          auto epoch = payload["editor"].toInt(-1);
          auto text = payload["text"].toString().toStdString();
          dispatcher_.schedule([this, epoch, cmd = std::move(cmd),
                                text = std::move(text)] {
            auto *ic = instance_->lastFocusedInputContext();
            if (epoch != editor_ || !ic || !ic->hasFocus() ||
                instance_->inputMethod(ic) != "vitral")
              return;
            if (cmd == "translate-selection" || cmd == "polish-selection") {
              inlineAi(ic, cmd == "translate-selection");
              return;
            }
            if (cmd == "toggle-language") {
              if (!internalEnglish_)
                return;
              auto action = unpack(vitral_process_key(core_, 0, 1u << 30,
                                                      VITRAL_SWIPE_DIR_NONE));
              english_ = action["lang_mode"].toString() == "en";
              render(ic, action);
              ic->updateUserInterface(UserInterfaceComponent::StatusArea);
              return;
            }
            if (cmd == "commit-text") {
              cancelPrediction(ic);
              if (composing_)
                unpack(vitral_process_key(core_, FcitxKey_Escape, 0,
                                         VITRAL_SWIPE_DIR_NONE));
              composing_ = false;
              hasPrev_ = hasNext_ = false;
              ic->inputPanel().reset();
              ic->updatePreedit();
              ic->updateUserInterface(UserInterfaceComponent::InputPanel);
              ic->commitString(text);
              schedulePrediction("live", predictionDelay_);
            } else {
              ic->inputPanel().setClientPreedit(
                  Text(text, TextFormatFlag::Underline));
            }
            ic->updatePreedit();
            ic->updateUserInterface(UserInterfaceComponent::InputPanel);
          });
        },
        [this] {
          return QJsonDocument(
                     QJsonObject{
                         {"active", active_.load()},
                         {"editor", editor_.load()},
                         {"english", english_.load()},
                         {"english_mode_enabled", internalEnglish_.load()},
                         {"theme", panelTheme_.load() == 1   ? "light"
                                   : panelTheme_.load() == 2 ? "dark"
                                                             : "system"},
                         {"activity", activity_.load()},
                         {"function", function_.load()},
                         {"function_serial", functionSerial_.load()},
                         {"function_editor", functionEditor_.load()}})
              .toJson(QJsonDocument::Compact)
              .toStdString();
        });
    startPanel();
  }

  void startPanel() {
    if (panelPid_ > 0 && ::kill(static_cast<pid_t>(panelPid_), 0) == 0)
      return;
    panelPid_ = 0;
    auto bin = QStandardPaths::findExecutable("vitral-panel");
    if (bin.isEmpty())
      return;
    auto environment = QProcessEnvironment::systemEnvironment();
    // KWin's private input-method socket belongs to Fcitx, not the panel.
    environment.remove("WAYLAND_SOCKET");
    QProcess proc;
    proc.setProgram(bin);
    proc.setProcessEnvironment(environment);
    qint64 pid = 0;
    if (proc.startDetached(&pid))
      panelPid_ = pid;
  }

  ~Engine() override {
    if (panelPid_ > 0)
      ::kill(static_cast<pid_t>(panelPid_), SIGTERM);
    cancelPrediction(nullptr);
    control_.reset();
    if (aiWorker_.joinable())
      aiWorker_.join();
    vitral_shutdown(core_);
  }

  void activate(const InputMethodEntry &, InputContextEvent &event) override {
    auto *ic = event.inputContext();
    ++editor_;
    active_ = true;
    ++activity_;
    startPanel();
    auto focus = unpack(vitral_on_app_focus(
        core_, ic->program().empty() ? "desktop" : ic->program().c_str()));
    english_ = focus["lang_mode"].toString() == "en";
    if (!internalEnglish_ && english_) {
      unpack(vitral_process_key(core_, 0, 1u << 30, VITRAL_SWIPE_DIR_NONE));
      english_ = false;
    }
    unpack(vitral_on_ime_open(core_));
    render(ic, command("candidate-state"));
    schedulePrediction("catch_up", 1);
  }

  void reset(const InputMethodEntry &, InputContextEvent &event) override {
    cancelPrediction(event.inputContext());
    lastCommitCursor_ = -1;
    lastCommitBefore_.clear();
    lastCommitAfter_.clear();
    if (composing_)
      unpack(
          vitral_process_key(core_, FcitxKey_Escape, 0, VITRAL_SWIPE_DIR_NONE));
    composing_ = false;
    shiftHeld_ = false;
    shortcutKeysDown_.clear();
    event.inputContext()->inputPanel().reset();
    event.inputContext()->updatePreedit();
    event.inputContext()->updateUserInterface(
        UserInterfaceComponent::InputPanel);
  }

  void deactivate(const InputMethodEntry &entry,
                  InputContextEvent &event) override {
    ++editor_;
    active_ = false;
    reset(entry, event);
    unpack(vitral_on_ime_close(core_));
  }

  void keyEvent(const InputMethodEntry &, KeyEvent &event) override {
    auto key = event.key();
    auto sym = key.sym();
    auto *ic = event.inputContext();
    if (!event.isRelease()) {
      if (!composing_ && !predictionText_.isEmpty() &&
          !key.states().test(KeyState::Ctrl) && !key.states().test(KeyState::Alt) &&
          !key.states().test(KeyState::Shift) && !key.states().test(KeyState::Super) &&
          sym == FcitxKey_Tab &&
          !ic->capabilityFlags().testAny(CapabilityFlag::PasswordOrSensitive) &&
          !hasSelection(ic) &&
          predictionContext_ == contextBefore(ic) &&
          predictionContextAfter_ == contextAfter(ic)) {
        const auto text = predictionText_;
        const auto id = predictionId_;
        const auto current = command("predict-poll", {{"id", id}});
        if (current["status"].toString() != "ready" || current["text"].toString() != text) {
          cancelPrediction(ic);
          return;
        }
        ic->commitString(text.toStdString());
        command("predict-accept", {{"id", id}, {"text", text}});
        predictionId_ = 0;
        cancelPrediction(ic);
        schedulePrediction("live", predictionDelay_);
        event.filterAndAccept();
        return;
      }
      if (sym == FcitxKey_Escape && !predictionText_.isEmpty()) {
        command("predict-dismiss", {{"id", predictionId_}});
        predictionId_ = 0;
        cancelPrediction(ic);
        event.filterAndAccept();
        return;
      }
      cancelPrediction(ic);
    }
    if (sym == FcitxKey_Shift_L || sym == FcitxKey_Shift_R) {
      if (key.states().test(KeyState::Ctrl) ||
          key.states().test(KeyState::Alt) ||
          key.states().test(KeyState::Super)) {
        shiftHeld_ = false;
        return;
      }
      if (event.isRelease() && shiftHeld_ && runShortcut(ic, "Shift"))
        event.filterAndAccept();
      shiftHeld_ = !event.isRelease();
      return;
    }
    shiftHeld_ = false;
    ++activity_;
    if (event.isRelease()) {
      if (shortcutKeysDown_.erase(sym))
        event.filterAndAccept();
      return;
    }
    if (shortcutKeysDown_.contains(sym)) {
      event.filterAndAccept();
      return;
    }
    QString shortcut;
    if (sym >= FcitxKey_F1 && sym <= FcitxKey_F12)
      shortcut = "F" + QString::number(sym - FcitxKey_F1 + 1);
    else if (key.states().test(KeyState::Ctrl) ||
             key.states().test(KeyState::Alt)) {
      if (sym >= FcitxKey_a && sym <= FcitxKey_z)
        shortcut = QChar(static_cast<char16_t>('A' + sym - FcitxKey_a));
      else if ((sym >= FcitxKey_A && sym <= FcitxKey_Z) ||
               (sym >= FcitxKey_0 && sym <= FcitxKey_9))
        shortcut = QChar(static_cast<char16_t>(sym));
      else if (sym == FcitxKey_space)
        shortcut = "Space";
      else if (sym == FcitxKey_Return)
        shortcut = "Enter";
    }
    if (!shortcut.isEmpty() && !key.states().test(KeyState::Super)) {
      QString prefix;
      if (key.states().test(KeyState::Ctrl))
        prefix += "Ctrl+";
      if (key.states().test(KeyState::Alt))
        prefix += "Alt+";
      if (key.states().test(KeyState::Shift))
        prefix += "Shift+";
      if (runShortcut(ic, prefix + shortcut)) {
        shortcutKeysDown_.insert(sym);
        event.filterAndAccept();
        return;
      }
    }
    if (!event.isRelease() && composing_ && sym >= FcitxKey_1 &&
        sym <= FcitxKey_5 &&
        (key.states().test(KeyState::Ctrl) ||
         key.states().test(KeyState::Alt))) {
      auto index = static_cast<size_t>(sym - FcitxKey_1);
      auto action = unpack(key.states().test(KeyState::Ctrl)
                               ? vitral_pin_candidate(core_, index)
                               : vitral_erase_candidate(core_, index));
      if (!action.isEmpty()) {
        render(ic, action);
        event.filterAndAccept();
      }
      return;
    }
    if (event.isRelease() || english_ || key.states().test(KeyState::Ctrl) ||
        key.states().test(KeyState::Alt) || key.states().test(KeyState::Super)) {
      if (!event.isRelease()) {
        lastCommitCursor_ = -1;
        lastCommitBefore_.clear();
        lastCommitAfter_.clear();
      }
      return;
    }
    if (!composing_ && sym == FcitxKey_BackSpace) {
      bool coreHandledIdleDelete = false;
      if (active_ && ic->hasFocus() && lastCommitCursor_ >= 0 &&
          lastCommitEditor_ == editor_.load() &&
          ic->capabilityFlags().test(CapabilityFlag::SurroundingText)) {
        const auto &surr = ic->surroundingText();
        const int expectedCursor = lastCommitCursor_;
        const auto expectedBefore = lastCommitBefore_;
        const auto expectedAfter = lastCommitAfter_;
        lastCommitCursor_ = -1;
        lastCommitBefore_.clear();
        lastCommitAfter_.clear();
        if (surr.isValid() && surr.cursor() == surr.anchor() &&
            static_cast<int>(surr.cursor()) == expectedCursor &&
            contextBefore(ic) == expectedBefore &&
            contextAfter(ic) == expectedAfter) {
          auto action =
              unpack(vitral_process_key(core_, sym, 0, VITRAL_SWIPE_DIR_NONE));
          const int undoChars = action["undo_commit_chars"].toInt(0);
          const auto undoText = action["undo_commit_text"].toString();
          if (undoChars > 0 && !undoText.isEmpty() &&
              expectedBefore.endsWith(undoText)) {
            ic->deleteSurroundingText(-undoChars, undoChars);
            render(ic, action);
            event.filterAndAccept();
            return;
          }
          if (undoChars > 0) {
            unpack(vitral_process_key(core_, FcitxKey_Escape, 0,
                                      VITRAL_SWIPE_DIR_NONE));
          } else {
            // Core idle BackSpace already recorded notify_deleted_chars(1).
            coreHandledIdleDelete = true;
          }
        }
      } else {
        lastCommitCursor_ = -1;
        lastCommitBefore_.clear();
        lastCommitAfter_.clear();
      }
      if (!coreHandledIdleDelete && active_ && ic->hasFocus() &&
          canReportIdleDeletion(ic->capabilityFlags()))
        command("notify-deleted-chars", {{"count", 1}});
      return;
    }
    auto list = std::dynamic_pointer_cast<CommonCandidateList>(
        ic->inputPanel().candidateList());
    if (list && list->size() > 0 && !key.states().test(KeyState::Shift)) {
      const bool vertical = sym == FcitxKey_Up || sym == FcitxKey_Down;
      if (vertical || (!composing_ && (sym == FcitxKey_Left || sym == FcitxKey_Right))) {
        const bool backward = sym == FcitxKey_Up || sym == FcitxKey_Left;
        const int next = list->cursorIndex() + (backward ? -1 : 1);
        if (vertical && (backward ? next < 0 && hasPrev_ : next >= list->size() && hasNext_)) {
          auto page = backward ? FcitxKey_Page_Up : FcitxKey_Page_Down;
          auto action =
              unpack(vitral_process_key(core_, page, 0, VITRAL_SWIPE_DIR_NONE));
          if (!action.isEmpty())
            render(ic, action, backward);
        } else {
          list->setGlobalCursorIndex(std::clamp(next, 0, list->size() - 1));
          ic->updateUserInterface(UserInterfaceComponent::InputPanel);
        }
        event.filterAndAccept();
        return;
      }
      if (sym == FcitxKey_space && list->cursorIndex() >= 0) {
        list->candidate(list->cursorIndex()).select(ic);
        event.filterAndAccept();
        return;
      }
    }
    const bool editingKey = sym == FcitxKey_BackSpace ||
                            sym == FcitxKey_Return || sym == FcitxKey_Escape ||
                            sym == FcitxKey_Left || sym == FcitxKey_Right ||
                            sym == FcitxKey_Home || sym == FcitxKey_End ||
                            sym == FcitxKey_Delete ||
                            sym == FcitxKey_Page_Up ||
                            sym == FcitxKey_Page_Down;
    if ((sym < 32 || sym > 126) && !(composing_ && editingKey)) {
      if (!composing_) {
        lastCommitCursor_ = -1;
        lastCommitBefore_.clear();
        lastCommitAfter_.clear();
      }
      return;
    }
    if (sym == FcitxKey_space && !composing_) {
      lastCommitCursor_ = -1;
      lastCommitBefore_.clear();
      lastCommitAfter_.clear();
      if (active_ && ic->hasFocus())
        unpack(vitral_process_key(core_, FcitxKey_space, 0,
                                  VITRAL_SWIPE_DIR_NONE));
      return;
    }
    bool before = composing_;
    auto action =
        unpack(vitral_process_key(core_, sym, 0, VITRAL_SWIPE_DIR_NONE));
    if (action.isEmpty())
      return;
    render(ic, action);
    if (before || composing_ || !action["commit_text"].toString().isEmpty())
      event.filterAndAccept();
  }

private:
  bool hasSelection(InputContext *ic) const {
    const auto &surrounding = ic->surroundingText();
    return surrounding.isValid() && surrounding.cursor() != surrounding.anchor();
  }
  QString contextBefore(InputContext *ic) const {
    const auto &surrounding = ic->surroundingText();
    if (!surrounding.isValid() ||
        ic->capabilityFlags().testAny(CapabilityFlag::PasswordOrSensitive))
      return {};
    const auto &text = surrounding.text();
    if (surrounding.cursor() == 0)
      return {};
    // Fcitx supplies a validated codepoint offset. Locate it without decoding
    // the document and convert only the UTF-8 suffix used for prediction.
    const auto cursor = utf8::nextNChar(text.cbegin(), surrounding.cursor());
    auto start = cursor;
    for (int n = 0; n < 160 && start != text.cbegin(); ++n) {
      do {
        --start;
      } while (utf8::isContinuationByte(*start));
    }
    return QString::fromUtf8(text.data() + (start - text.cbegin()), cursor - start);
  }
  QString contextAfter(InputContext *ic) const {
    const auto &surrounding = ic->surroundingText();
    if (!surrounding.isValid() ||
        ic->capabilityFlags().testAny(CapabilityFlag::PasswordOrSensitive))
      return {};
    const auto &text = surrounding.text();
    const auto cursor = utf8::nextNChar(text.data(), surrounding.cursor());
    const auto end = text.data() + text.size();
    auto finish = cursor;
    for (int n = 0; n < 80 && finish != end; ++n)
      finish = utf8::nextChar(finish);
    return QString::fromUtf8(cursor, finish - cursor);
  }
  void cancelSubmittedPrediction() {
    if (predictionId_ != 0) {
      command("predict-cancel");
      predictionId_ = 0;
    }
  }
  void cancelPrediction(InputContext *ic) {
    predictionTimer_.reset();
    cancelSubmittedPrediction();
    const bool visible = !predictionText_.isEmpty();
    predictionText_.clear();
    predictionHint_.clear();
    if (ic && visible) {
      ic->inputPanel().setAuxDown(Text());
      ic->updateUserInterface(UserInterfaceComponent::InputPanel);
    }
  }
  void schedulePrediction(const char *trigger, int delay) {
    if (!predictionEnabled_)
      return;
    cancelPrediction(instance_->lastFocusedInputContext());
    const int epoch = editor_.load();
    predictionPolls_ = 0;
    predictionTimer_ = instance_->eventLoop().addTimeEvent(
        CLOCK_MONOTONIC, now(CLOCK_MONOTONIC) + delay * 1000, 0,
        [this, epoch, trigger](EventSourceTime *timer, uint64_t) {
          timer->setEnabled(false);
          auto *ic = instance_->lastFocusedInputContext();
          if (epoch != editor_ || !active_ || composing_ || !ic || !ic->hasFocus() ||
              instance_->inputMethod(ic) != "vitral" || hasSelection(ic) ||
              ic->capabilityFlags().testAny(CapabilityFlag::PasswordOrSensitive)) {
            cancelSubmittedPrediction();
            return true;
          }
          QJsonObject response;
          const bool submitting = predictionId_ == 0;
          if (submitting) {
            predictionContext_ = contextBefore(ic);
            predictionContextAfter_ = contextAfter(ic);
            response = command("predict-request", {
                {"app_id", QString::fromStdString(ic->program().empty() ? "desktop" : ic->program())},
                {"context_before", predictionContext_},
                {"context_after", predictionContextAfter_},
                {"trigger", trigger},
                {"is_composing", false},
                {"is_password", false},
            });
          } else {
            response = command("predict-poll", {{"id", predictionId_}});
          }
          const auto status = response["status"].toString();
          if (submitting && (status == "pending" || status == "ready"))
            predictionId_ = response["id"].toDouble();
          ++predictionPolls_;
          if (status == "pending" && predictionPolls_ <= 25) {
            timer->setNextInterval(120000);
            timer->setOneShot();
            return true;
          }
          if (status == "ready" && predictionContext_ == contextBefore(ic) &&
              predictionContextAfter_ == contextAfter(ic)) {
            predictionText_ = response["text"].toString();
            if (!predictionText_.isEmpty()) {
              predictionHint_ = "Tab 补全：" + predictionText_;
              const auto candidates = response["candidates"].toArray();
              if (candidates.size() > 1) {
                QStringList alternatives;
                for (qsizetype i = 1; i < candidates.size() && i < 4; ++i) {
                  auto alternative = candidates[i].toString();
                  const auto characters = alternative.toUcs4();
                  if (characters.size() > 8)
                    alternative = QString::fromUcs4(characters.constData(), 8) + "…";
                  alternatives.append(alternative);
                }
                predictionHint_ += " · 备选 " + alternatives.join(" / ");
              }
              ic->inputPanel().setAuxDown(Text(predictionHint_.toStdString()));
              ic->updateUserInterface(UserInterfaceComponent::InputPanel);
              return true;
            }
          }
          if (status == "pending" || status == "ready")
            cancelSubmittedPrediction();
          predictionId_ = 0;
          return true;
        });
    predictionTimer_->setOneShot();
  }
  bool runShortcut(InputContext *ic, const QString &key) {
    for (const auto &item : shortcuts_) {
      auto binding = item.toObject();
      if (binding["key"].toString() != key ||
          (!internalEnglish_ &&
           binding["action"].toString() == "toggle_language"))
        continue;
      auto action = command("shortcut-key", {{"key", key}});
      if (action.isEmpty())
        return false;
      render(ic, action);
      return true;
    }
    return false;
  }
  QJsonObject command(const char *name, const QJsonObject &args = {}) {
    auto bytes = QJsonDocument(args).toJson(QJsonDocument::Compact);
    return unpack(vitral_fcitx_command(core_, name, bytes.constData()));
  }
  void inlineAi(InputContext *ic, bool translate) {
    auto show = [](InputContext *context, const std::string &message) {
      context->inputPanel().setAuxDown(Text(message));
      context->updateUserInterface(UserInterfaceComponent::InputPanel);
    };
    auto source = ic->surroundingText().selectedText();
    if (source.empty()) {
      show(ic, "请先在输入框选中原文");
      return;
    }
    if (aiBusy_) {
      show(ic, "正在处理选中文本…");
      return;
    }
    if (aiWorker_.joinable())
      aiWorker_.join();
    aiBusy_ = true;
    show(ic, translate ? "正在翻译…" : "正在润色…");
    aiWorker_ = std::jthread([this, source, epoch = editor_.load(),
                              tick = activity_.load(), translate, show] {
      auto response = vitral_ai_inline(
          core_, translate ? VITRAL_AI_MODE_TRANSLATE : VITRAL_AI_MODE_POLISH,
          source.c_str(), translate ? "en" : "zh");
      std::string error = response.error_message ? response.error_message : "";
      auto text = unpack(response)["output_text"].toString();
      dispatcher_.schedule([this, epoch, tick, source, error, text, show] {
        aiBusy_ = false;
        auto *context = instance_->lastFocusedInputContext();
        if (!context || !context->hasFocus() || editor_ != epoch ||
            activity_ != tick ||
            source != context->surroundingText().selectedText())
          return;
        if (text.isEmpty()) {
          show(context, error.empty() ? "未收到结果" : error);
          return;
        }
        auto list = std::make_unique<CommonCandidateList>();
        list->append<Choice>(
            QJsonObject{{"text", text}, {"index", 0}},
            [this, epoch, source, text](InputContext *target) {
              if (editor_ == epoch &&
                  target->surroundingText().selectedText() == source)
                target->commitString(text.toStdString());
              target->inputPanel().reset();
              target->updateUserInterface(UserInterfaceComponent::InputPanel);
            });
        context->inputPanel().setCandidateList(std::move(list));
        show(context, "点击结果替换选中文本");
      });
    });
  }

  void setPanelTheme(const QJsonObject &settings) {
    const auto theme = settings["theme"].toString();
    panelTheme_ = theme == "light" ? 1 : theme == "dark" ? 2 : 0;
  }
  void applyAppearance(const QJsonObject &settings) {
    setPanelTheme(settings);
    predictionDelay_ = std::clamp(settings["prediction_delay_ms"].toInt(380), 150, 2000);
    predictionEnabled_ = settings["prediction_enabled"].toBool(true);
    cancelPrediction(instance_->lastFocusedInputContext());
    internalEnglish_ = settings["linux_english_mode"].toBool(true);
    shortcuts_ = command("shortcuts-state")["bindings"].toArray();
    shiftHeld_ = false;
    auto *ic = instance_->lastFocusedInputContext();
    if (!internalEnglish_ && english_) {
      auto action =
          unpack(vitral_process_key(core_, 0, 1u << 30, VITRAL_SWIPE_DIR_NONE));
      english_ = false;
      if (active_ && ic && ic->hasFocus() &&
          instance_->inputMethod(ic) == "vitral") {
        render(ic, action);
        ic->updateUserInterface(UserInterfaceComponent::StatusArea);
      }
    }
    auto *ui = instance_->addonManager().addon("classicui");
    if (!ui || !ui->getConfig())
      return;
    RawConfig cfg;
    ui->getConfig()->save(cfg);
    auto theme = settings["theme"].toString("system");
    cfg["Theme"] = theme == "dark" ? "Vitral" : "Vitral-Light";
    cfg["DarkTheme"] = "Vitral";
    cfg["UseDarkTheme"] = theme == "system" ? "True" : "False";
    auto points =
        std::clamp(settings["candidate_font_px"].toInt(16), 13, 22) * 0.75;
    cfg["Font"] = "Sans " + QString::number(points).toStdString();
    ui->setConfig(cfg);
  }

  std::string subModeLabelImpl(const InputMethodEntry &,
                               InputContext &) override {
    return english_ ? "En" : "中";
  }
  void render(InputContext *ic, const QJsonObject &action, bool selectLast = false) {
    english_ = action["lang_mode"].toString() == "en";
    auto commit = action["commit_text"].toString();
    if (!commit.isEmpty()) {
      const auto commitUtf8 = commit.toStdString();
      const auto &surr = ic->surroundingText();
      if (composing_ &&
          ic->capabilityFlags().test(CapabilityFlag::SurroundingText) &&
          surr.isValid()) {
        const auto start = std::min(surr.cursor(), surr.anchor());
        lastCommitCursor_ =
            static_cast<int>(start + utf8::length(commitUtf8));
        lastCommitEditor_ = editor_.load();
        // Capture full surrounding snippet so Wayland per-paragraph offsets cannot alias another paragraph.
        auto beforeNow = contextBefore(ic);
        beforeNow.append(commit);
        if (beforeNow.size() > 160)
          beforeNow = beforeNow.right(160);
        lastCommitBefore_ = beforeNow;
        lastCommitAfter_ = contextAfter(ic);
      } else {
        lastCommitCursor_ = -1;
        lastCommitBefore_.clear();
        lastCommitAfter_.clear();
      }
      ic->commitString(commitUtf8);
    }
    auto preedit = action["preedit"].toObject();
    auto text = preedit["text"].toString().toStdString();
    composing_ = !text.empty();
    if (composing_) cancelPrediction(ic);
    else if (!commit.isEmpty()) schedulePrediction("live", predictionDelay_);
    const auto page = action["page"].toObject();
    hasPrev_ = page["has_prev"].toBool();
    hasNext_ = page["has_next"].toBool();
    auto &panel = ic->inputPanel();
    panel.reset();
    if (!composing_ && !predictionHint_.isEmpty())
      panel.setAuxDown(Text(predictionHint_.toStdString()));
    Text marked(text, TextFormatFlag::Underline);
    marked.setCursor(std::clamp(preedit["cursor_utf8"].toInt(), 0,
                                static_cast<int>(text.size())));
    if (ic->capabilityFlags().test(CapabilityFlag::Preedit))
      panel.setClientPreedit(marked);
    else
      panel.setPreedit(marked);
    auto list = std::make_unique<PagedCandidateList>(
        hasPrev_, hasNext_, [this, ic](KeySym key) {
          auto action =
              unpack(vitral_process_key(core_, key, 0, VITRAL_SWIPE_DIR_NONE));
          if (!action.isEmpty())
            render(ic, action);
        });
    list->setPageSize(ergonomics::kDesktopCandidatePageSize);
    list->setLayoutHint(CandidateLayoutHint::Horizontal);
    for (const auto &item : action["candidates"].toArray()) {
      auto candidate = item.toObject();
      list->append<Choice>(
          candidate,
          [this, index = candidate["index"].toInt()](InputContext *context) {
            render(context, unpack(vitral_select_candidate(core_, index)));
          });
    }
    if (list->size()) {
      list->setGlobalCursorIndex(selectLast ? list->size() - 1 : 0);
      panel.setCandidateList(std::move(list));
    }
    ic->updatePreedit();
    ic->updateUserInterface(UserInterfaceComponent::InputPanel);
    const auto function = action["function_action"].toString();
    if (!function.isEmpty())
      ic->updateUserInterface(UserInterfaceComponent::StatusArea);
    if (function == "settings")
      QProcess::startDetached("vitral-desktop", {});
    else if (function == "clipboard" || function == "lookup" ||
             function == "handwriting") {
      function_ = function == "clipboard" ? 1 : function == "lookup" ? 2 : 3;
      functionEditor_ = editor_.load();
      ++functionSerial_;
    }
  }

  Instance *instance_;
  EventDispatcher dispatcher_;
  std::unique_ptr<EventSourceTime> predictionTimer_;
  QString predictionText_, predictionHint_, predictionContext_, predictionContextAfter_;
  double predictionId_ = 0;
  int predictionDelay_ = 380, predictionPolls_ = 0;
  int lastCommitCursor_ = -1, lastCommitEditor_ = 0;
  QString lastCommitBefore_, lastCommitAfter_;
  bool predictionEnabled_ = true;
  std::unique_ptr<DesktopControl> control_;
  VitralEngine *core_ = nullptr;
  std::jthread aiWorker_;
  bool aiBusy_ = false;
  bool composing_ = false, hasPrev_ = false, hasNext_ = false;
  qint64 panelPid_ = 0;
  std::atomic<bool> english_{false}, active_{false};
  std::atomic<bool> internalEnglish_{true};
  std::atomic<int> activity_{0}, editor_{0}, panelTheme_{0};
  bool shiftHeld_ = false;
  QJsonArray shortcuts_;
  std::set<uint32_t> shortcutKeysDown_;
  std::atomic<int> function_{0}, functionSerial_{0}, functionEditor_{-1};
};

class Factory final : public AddonFactory {
  AddonInstance *create(AddonManager *manager) override {
    return new Engine(manager->instance());
  }
};
} // namespace
FCITX_ADDON_FACTORY_V2_BACKWARDS(vitral, Factory)
