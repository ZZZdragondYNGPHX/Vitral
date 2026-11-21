#include "control.hpp"
#include <QFile>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QProcess>
#include <QSettings>
#include <QStandardPaths>
#include <QSysInfo>
#include <QUuid>
#include <atomic>
#include <fcitx-config/configuration.h>
#include <fcitx-config/rawconfig.h>
#include <fcitx-utils/eventdispatcher.h>
#include <fcitx-utils/log.h>
#include <fcitx/addonfactory.h>
#include <fcitx/addonmanager.h>
#include <fcitx/candidatelist.h>
#include <fcitx/event.h>
#include <fcitx/inputcontext.h>
#include <fcitx/inputmethodengine.h>
#include <fcitx/inputpanel.h>
#include <fcitx/instance.h>
#include <functional>
#include <stdexcept>

#include "vitral_core.h"
#include "vitral_tokens.hpp"

namespace {
using namespace fcitx;
using namespace vitral::tokens;

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
  Choice(const QJsonObject &candidate, Select select)
      : CandidateWord(Text(candidate["text"].toString().toStdString())),
        select_(std::move(select)) {
    setComment(Text(candidate["comment"].toString().toStdString()));
    setCustomLabel(Text(std::to_string(candidate["index"].toInt() + 1) + ". "));
  }
  void select(InputContext *ic) const override {
    auto callback = select_;
    callback(ic);
  }

private:
  Select select_;
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
              auto action = unpack(vitral_process_key(core_, 0, 1u << 30,
                                                      VITRAL_SWIPE_DIR_NONE));
              english_ = action["lang_mode"].toString() == "en";
              render(ic, action);
              return;
            }
            if (cmd == "commit-text") {
              ic->commitString(text);
              ic->inputPanel().reset();
            } else {
              ic->inputPanel().setClientPreedit(
                  Text(text, TextFormatFlag::Underline));
            }
            ic->updatePreedit();
            ic->updateUserInterface(UserInterfaceComponent::InputPanel);
          });
        },
        [this] {
          return QJsonDocument(QJsonObject{{"active", active_.load()},
                                           {"editor", editor_.load()},
                                           {"english", english_.load()},
                                           {"activity", activity_.load()}})
              .toJson(QJsonDocument::Compact)
              .toStdString();
        });
  }

  ~Engine() override {
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
    if (panel_.state() == QProcess::NotRunning) {
      auto bin = QStandardPaths::findExecutable("vitral-panel");
      if (!bin.isEmpty())
        panel_.start(bin, {});
    }
    auto focus = unpack(vitral_on_app_focus(
        core_, ic->program().empty() ? "desktop" : ic->program().c_str()));
    english_ = focus["lang_mode"].toString() == "en";
    unpack(vitral_on_ime_open(core_));
  }

  void reset(const InputMethodEntry &, InputContextEvent &event) override {
    if (composing_)
      unpack(
          vitral_process_key(core_, FcitxKey_Escape, 0, VITRAL_SWIPE_DIR_NONE));
    composing_ = false;
    shiftHeld_ = false;
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
    if (sym == FcitxKey_Shift_L || sym == FcitxKey_Shift_R) {
      if (event.isRelease() && shiftHeld_) {
        auto action = unpack(
            vitral_process_key(core_, 0, 1u << 30, VITRAL_SWIPE_DIR_NONE));
        english_ = action["lang_mode"].toString() == "en";
        render(ic, action);
        event.filterAndAccept();
      }
      shiftHeld_ = !event.isRelease();
      return;
    }
    shiftHeld_ = false;
    ++activity_;
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
        key.states().test(KeyState::Alt) || key.states().test(KeyState::Super))
      return;
    const bool editingKey = sym == FcitxKey_BackSpace ||
                            sym == FcitxKey_Return || sym == FcitxKey_Escape ||
                            sym == FcitxKey_Page_Up ||
                            sym == FcitxKey_Page_Down;
    if ((sym < 32 || sym > 126) && !(composing_ && editingKey))
      return;
    if (sym == FcitxKey_space && !composing_)
      return;
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

  void applyAppearance(const QJsonObject &settings) {
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
  void render(InputContext *ic, const QJsonObject &action) {
    auto commit = action["commit_text"].toString();
    if (!commit.isEmpty())
      ic->commitString(commit.toStdString());
    auto preedit = action["preedit"].toObject();
    auto text = preedit["text"].toString().toStdString();
    composing_ = !text.empty();
    auto &panel = ic->inputPanel();
    panel.reset();
    Text marked(text, TextFormatFlag::Underline);
    marked.setCursor(preedit["cursor_utf8"].toInt());
    if (ic->capabilityFlags().test(CapabilityFlag::Preedit))
      panel.setClientPreedit(marked);
    else
      panel.setPreedit(marked);
    auto list = std::make_unique<CommonCandidateList>();
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
      list->setGlobalCursorIndex(0);
      panel.setCandidateList(std::move(list));
    }
    ic->updatePreedit();
    ic->updateUserInterface(UserInterfaceComponent::InputPanel);
  }

  Instance *instance_;
  EventDispatcher dispatcher_;
  std::unique_ptr<DesktopControl> control_;
  VitralEngine *core_ = nullptr;
  std::jthread aiWorker_;
  bool aiBusy_ = false;
  bool composing_ = false;
  QProcess panel_;
  std::atomic<bool> english_{false}, active_{false};
  std::atomic<int> activity_{0}, editor_{0};
  bool shiftHeld_ = false;
};

class Factory final : public AddonFactory {
  AddonInstance *create(AddonManager *manager) override {
    return new Engine(manager->instance());
  }
};
} // namespace
FCITX_ADDON_FACTORY_V2_BACKWARDS(vitral, Factory)
