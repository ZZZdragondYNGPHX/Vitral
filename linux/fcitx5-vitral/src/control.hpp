#pragma once

#include "vitral_core.h"
#include <cerrno>
#include <chrono>
#include <condition_variable>
#include <cstdlib>
#include <cstring>
#include <fcitx-utils/log.h>
#include <fcitx-utils/unixfd.h>
#include <functional>
#include <list>
#include <mutex>
#include <string>
#include <sys/eventfd.h>
#include <systemd/sd-bus.h>
#include <systemd/sd-event.h>
#include <thread>

extern "C" VitralStringResult vitral_fcitx_command(VitralEngine *, const char *,
                                                   const char *);

// Management calls run off Fcitx's input event loop. The bus supplies the
// caller's session isolation, and the engine stays alive until the worker
// joins.
class DesktopControl {
public:
  using Present = std::function<void(std::string, std::string)>;
  DesktopControl(VitralEngine *core, Present present,
                 std::function<std::string()> state)
      : core_(core), present_(std::move(present)), state_(std::move(state)),
        thread_([this](std::stop_token stop) { serve(stop); }) {}

private:
  struct ServiceCall {
    sd_bus_message *message;
    std::string command;
    std::string args;
    VitralStringResult result{};
  };

  static int dispatch(sd_bus_message *msg, void *userdata,
                      sd_bus_error *error) {
    auto &self = *static_cast<DesktopControl *>(userdata);
    const char *cmd = nullptr, *args = nullptr;
    int rc = sd_bus_message_read(msg, "ss", &cmd, &args);
    if (rc < 0)
      return rc;
    try {
      std::string command(cmd);
      if (command == "panel-state")
        return sd_bus_reply_method_return(msg, "s", self.state_().c_str());
      const bool fullSync = command == "poll-services" || command == "clipboard-poll";
      const bool asyncNetworkOrHeavy =
          command == "storage" || command == "relay-send" ||
          command == "relay-poll-inbox" || command == "relay-cleanup-orphans" ||
          command == "relay-discover" || command == "restore-from-koofr" ||
          command == "models-download" || command == "models-refresh-catalog" ||
          command == "ai-inline" || command == "profile-distill" ||
          command == "voice-preview" || command == "voice-stop" ||
          command == "import-lexicon";
      if (fullSync || asyncNetworkOrHeavy ||
          command == "clipboard-poll-current" || command == "clipboard-push" ||
          command == "clipboard-pin" || command == "clipboard-unpin" ||
          command == "clipboard-clear-unpinned" ||
          command == "models-cancel" || command == "models-delete") {
        {
          std::lock_guard lock(self.serviceMutex_);
          auto &queue = fullSync             ? self.fullSyncQueue_
                        : asyncNetworkOrHeavy ? self.asyncQueue_
                                              : self.queued_;
          queue.emplace_back(msg, std::move(command), args);
          sd_bus_message_ref(msg);
        }
        self.serviceReady_.notify_all();
        return 1; // The bus thread replies after the service finishes.
      }
      if (command == "translate-selection" || command == "polish-selection" ||
          command == "toggle-language" || command == "commit-text" ||
          command == "set-preedit") {
        self.present_(command, args);
        return sd_bus_reply_method_return(msg, "s", "{}");
      }
      auto result = vitral_fcitx_command(self.core_, cmd, args);
      if (result.status == VITRAL_STATUS_OK) {
        if (command == "configure")
          self.present_(command, args);
        rc = sd_bus_reply_method_return(msg, "s", result.json_utf8);
      } else
        rc = sd_bus_error_setf(error, "dev.vitral.IME.Error", "%s",
                               result.error_message ? result.error_message
                                                    : "command failed");
      vitral_free_result(result);
      return rc;
    } catch (const std::exception &ex) {
      return sd_bus_error_setf(error, "dev.vitral.IME.Error", "%s", ex.what());
    }
  }

  void runServices(std::stop_token stop, int wake, std::list<ServiceCall> &queue) {
    std::unique_lock lock(serviceMutex_);
    std::list<ServiceCall> running;
    while (serviceReady_.wait(lock, stop, [&queue] { return !queue.empty(); })) {
      if (stop.stop_requested())
        break;
      running.splice(running.end(), queue, queue.begin());
      lock.unlock();
      auto &call = running.front();
      call.result = vitral_fcitx_command(core_, call.command.c_str(), call.args.c_str());
      lock.lock();
      completed_.splice(completed_.end(), running);
      lock.unlock();
      int rc;
      do {
        rc = eventfd_write(wake, 1);
      } while (rc < 0 && errno == EINTR);
      if (rc < 0 && errno != EAGAIN)
        FCITX_WARN() << "Vitral management wake: " << strerror(errno);
      lock.lock();
    }
  }

  void replyCompleted() {
    std::list<ServiceCall> ready;
    {
      std::lock_guard lock(serviceMutex_);
      ready.swap(completed_);
    }
    // Message references and replies stay on the bus thread.
    for (auto &call : ready) {
      auto &result = call.result;
      const int rc = result.status == VITRAL_STATUS_OK
                         ? sd_bus_reply_method_return(call.message, "s", result.json_utf8)
                         : sd_bus_reply_method_errorf(call.message, "dev.vitral.IME.Error", "%s",
                               result.error_message ? result.error_message : "command failed");
      if (rc < 0)
        FCITX_WARN() << "Vitral management reply: " << strerror(-rc);
      vitral_free_result(result);
      sd_bus_message_unref(call.message);
    }
  }

  static int onServiceReady(sd_event_source *, int wake, uint32_t, void *userdata) {
    eventfd_t ready;
    int rc;
    do {
      rc = eventfd_read(wake, &ready);
    } while (rc < 0 && errno == EINTR);
    if (rc < 0 && errno != EAGAIN)
      return -errno;
    static_cast<DesktopControl *>(userdata)->replyCompleted();
    return 0;
  }

  void notifyClipboard(sd_bus *bus, uint64_t &revision) {
    auto result = vitral_fcitx_command(core_, "clipboard-lan-revision", "{}");
    if (result.status == VITRAL_STATUS_OK) {
      const auto next = std::strtoull(result.json_utf8, nullptr, 10);
      if (next && next != revision &&
          sd_bus_emit_signal(bus, "/dev/vitral/IME", "dev.vitral.IME",
                             "ClipboardChanged", "") >= 0)
        revision = next;
    }
    vitral_free_result(result);
  }

  void serve(std::stop_token stop) {
    static const sd_bus_vtable methods[] = {
        SD_BUS_VTABLE_START(0),
        SD_BUS_METHOD("Command", "ss", "s", dispatch,
                      SD_BUS_VTABLE_UNPRIVILEGED),
        SD_BUS_SIGNAL("ClipboardChanged", "", 0),
        SD_BUS_VTABLE_END};
    sd_bus *bus = nullptr;
    sd_bus_slot *slot = nullptr;
    sd_event *event = nullptr;
    sd_event_source *source = nullptr;
    const auto wake = fcitx::UnixFD::own(eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK));
    int rc = wake.isValid() ? sd_event_new(&event) : -errno;
    if (rc >= 0)
      rc = sd_event_add_io(event, &source, wake.fd(), EPOLLIN, onServiceReady, this);
    if (rc >= 0)
      rc = sd_bus_open_user(&bus);
    if (rc >= 0)
      rc = sd_bus_add_object_vtable(bus, &slot, "/dev/vitral/IME",
                                    "dev.vitral.IME", methods, this);
    if (rc >= 0)
      rc = sd_bus_request_name(bus, "dev.vitral.IME", 0);
    if (rc >= 0)
      rc = sd_bus_attach_event(bus, event, 0);
    if (rc >= 0) {
      std::jthread service([this, fd = wake.fd()](std::stop_token stop) {
        runServices(stop, fd, queued_);
      });
      std::jthread fullSync([this, fd = wake.fd()](std::stop_token stop) {
        runServices(stop, fd, fullSyncQueue_);
      });
      std::jthread asyncWork([this, fd = wake.fd()](std::stop_token stop) {
        runServices(stop, fd, asyncQueue_);
      });
      using Clock = std::chrono::steady_clock;
      constexpr auto clipboardInterval = std::chrono::milliseconds(40);
      auto nextClipboardCheck = Clock::now();
      uint64_t clipboardRevision = 0;
      while (rc >= 0 && !stop.stop_requested()) {
        auto now = Clock::now();
        if (now >= nextClipboardCheck) {
          notifyClipboard(bus, clipboardRevision);
          nextClipboardCheck = now + clipboardInterval;
        }
        auto timeout = std::chrono::duration_cast<std::chrono::microseconds>(
            nextClipboardCheck - now);
        rc = sd_event_run(event, timeout.count());
      }
      service.request_stop();
      fullSync.request_stop();
      asyncWork.request_stop();
      service.join();
      fullSync.join();
      asyncWork.join();
      replyCompleted();
      std::list<ServiceCall> pending;
      {
        std::lock_guard lock(serviceMutex_);
        pending.swap(queued_);
        pending.splice(pending.end(), fullSyncQueue_);
        pending.splice(pending.end(), asyncQueue_);
      }
      for (auto &call : pending) {
        sd_bus_reply_method_errorf(call.message, "dev.vitral.IME.Error",
                                   "shutting down");
        sd_bus_message_unref(call.message);
      }
    }
    if (rc < 0)
      FCITX_WARN() << "Vitral management bus: " << strerror(-rc);
    sd_bus_slot_unref(slot);
    sd_bus_flush_close_unref(bus);
    sd_event_source_unref(source);
    sd_event_unref(event);
  }

  VitralEngine *core_;
  Present present_;
  std::function<std::string()> state_;
  std::mutex serviceMutex_;
  std::condition_variable_any serviceReady_;
  std::list<ServiceCall> queued_, fullSyncQueue_, asyncQueue_, completed_;
  std::jthread thread_;
};
