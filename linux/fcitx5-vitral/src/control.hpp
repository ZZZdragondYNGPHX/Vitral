#pragma once

#include "vitral_core.h"
#include <cstring>
#include <fcitx-utils/log.h>
#include <functional>
#include <string>
#include <systemd/sd-bus.h>
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

  void serve(std::stop_token stop) {
    static const sd_bus_vtable methods[] = {
        SD_BUS_VTABLE_START(0),
        SD_BUS_METHOD("Command", "ss", "s", dispatch,
                      SD_BUS_VTABLE_UNPRIVILEGED),
        SD_BUS_VTABLE_END};
    sd_bus *bus = nullptr;
    sd_bus_slot *slot = nullptr;
    int rc = sd_bus_open_user(&bus);
    if (rc >= 0)
      rc = sd_bus_add_object_vtable(bus, &slot, "/dev/vitral/IME",
                                    "dev.vitral.IME", methods, this);
    if (rc >= 0)
      rc = sd_bus_request_name(bus, "dev.vitral.IME", 0);
    while (rc >= 0 && !stop.stop_requested()) {
      rc = sd_bus_process(bus, nullptr);
      if (rc == 0)
        rc = sd_bus_wait(bus, 250000);
    }
    if (rc < 0)
      FCITX_WARN() << "Vitral management bus: " << strerror(-rc);
    sd_bus_slot_unref(slot);
    sd_bus_flush_close_unref(bus);
  }

  VitralEngine *core_;
  Present present_;
  std::function<std::string()> state_;
  std::jthread thread_;
};
