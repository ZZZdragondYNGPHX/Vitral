#pragma once

#include "qwayland-ext-data-control-v1.h"
#include <QGuiApplication>
#include <QDateTime>
#include <QPointer>
#include <csignal>
#include <QSocketNotifier>
#include <QWaylandClientExtension>
#include <QtGui/qguiapplication_platform.h>
#include <fcntl.h>
#include <functional>
#include <map>
#include <memory>
#include <unistd.h>

// Qt's built-in clipboard supports wlr-data-control; newer compositors may
// advertise only ext-data-control. Both feed the same clipboard-push service.
class WaylandClipboard final
    : public QWaylandClientExtensionTemplate<WaylandClipboard>,
      public QtWayland::ext_data_control_manager_v1 {
  struct Offer final : QtWayland::ext_data_control_offer_v1 {
    explicit Offer(::ext_data_control_offer_v1 *offer)
        : QtWayland::ext_data_control_offer_v1(offer) {}
    ~Offer() override { destroy(); }
    QString mime;
    QString copyId;
    void ext_data_control_offer_v1_offer(const QString &type) override {
      if (type.startsWith("application/x-vitral-copy-id-"))
        copyId = QString::fromUtf8(QByteArray::fromHex(type.mid(QString("application/x-vitral-copy-id-").size()).toLatin1()));
      if (type == "text/plain;charset=utf-8" ||
          (mime.isEmpty() && type == "text/plain"))
        mime = type;
    }
  };
  struct Transfer final : QObject {
    int fd;
    QByteArray data;
    qsizetype offset = 0;
    QSocketNotifier notifier;
    Transfer(int fd, QByteArray data, QObject *parent)
        : QObject(parent), fd(fd), data(std::move(data)), notifier(fd, QSocketNotifier::Write, this) {
      fcntl(fd, F_SETFL, O_NONBLOCK);
      connect(&notifier, &QSocketNotifier::activated, this, [this] {
        auto n = write(this->fd, this->data.constData() + offset, this->data.size() - offset);
        if (n > 0) offset += n;
        if (offset == this->data.size() || (n < 0 && errno != EAGAIN && errno != EINTR)) {
          notifier.setEnabled(false);
          deleteLater();
        }
      });
    }
    ~Transfer() override { close(fd); }
  };
  struct Source final : QObject, QtWayland::ext_data_control_source_v1 {
    QByteArray text, id;
    Source(::ext_data_control_source_v1 *source, const QString &text, const QString &id, QObject *parent)
        : QObject(parent), QtWayland::ext_data_control_source_v1(source), text(text.toUtf8()), id(id.toUtf8()) {
      offer("text/plain;charset=utf-8");
      offer("text/plain");
      offer("application/x-vitral-copy-id-" + QString::fromLatin1(this->id.toHex()));
    }
    ~Source() override { destroy(); }
    void ext_data_control_source_v1_send(const QString &mime, int32_t fd) override {
      new Transfer(fd, mime.startsWith("text/plain") ? text : id, parent());
    }
    void ext_data_control_source_v1_cancelled() override { deleteLater(); }
  };
  struct Device final : QtWayland::ext_data_control_device_v1 {
    WaylandClipboard &owner;
    std::map<::ext_data_control_offer_v1 *, std::unique_ptr<Offer>> offers;
    Device(WaylandClipboard &owner, ::ext_data_control_device_v1 *device)
        : QtWayland::ext_data_control_device_v1(device), owner(owner) {}
    ~Device() override { destroy(); }
    void ext_data_control_device_v1_data_offer(
        ::ext_data_control_offer_v1 *offer) override {
      offers.emplace(offer, std::make_unique<Offer>(offer));
    }
    void ext_data_control_device_v1_selection(
        ::ext_data_control_offer_v1 *offer) override {
      owner.cancelRead();
      auto selected = offers.find(offer);
      const auto stamp = QDateTime::currentMSecsSinceEpoch();
      if (selected != offers.end() && !selected->second->mime.isEmpty()) {
        // Invalidate pending cloud reads before the asynchronous text transfer.
        if (selected->second->copyId.isEmpty()) owner.copied_({}, {}, stamp);
        owner.receive(*selected->second, stamp);
      } else {
        owner.copied_({}, {}, offer ? stamp : 0);
      }
      // receive has sent the fd; protocol objects can now be released.
      offers.erase(offer);
    }
    void ext_data_control_device_v1_primary_selection(
        ::ext_data_control_offer_v1 *offer) override {
      offers.erase(offer);
    }
    void ext_data_control_device_v1_finished() override { owner.cancelRead(); }
  };

public:
  explicit WaylandClipboard(std::function<void(QString, QString, qint64)> copied)
      : QWaylandClientExtensionTemplate(1), copied_(std::move(copied)) {
    connect(this, &QWaylandClientExtension::activeChanged, this, [this] {
      auto *native =
          qGuiApp->nativeInterface<QNativeInterface::QWaylandApplication>();
      if (isActive() && native && native->seat())
        device_ =
            std::make_unique<Device>(*this, get_data_device(native->seat()));
      else
        device_.reset();
    });
    std::signal(SIGPIPE, SIG_IGN);
    initialize();
  }
  bool setText(const QString &text, const QString &id) {
    if (!isActive() || !device_) return false;
    auto *source = new Source(create_data_source(), text, id, this);
    device_->set_selection(source->object());
    return true;
  }
  ~WaylandClipboard() override {
    cancelRead();
    device_.reset();
    if (isActive())
      destroy();
  }

private:
  void cancelRead() {
    if (notifier_) {
      notifier_->setEnabled(false);
      disconnect(notifier_.get(), nullptr, this, nullptr);
      notifier_.release()->deleteLater();
    }
    if (fd_ >= 0)
      close(fd_);
    fd_ = -1;
    bytes_.clear();
  }
  void receive(Offer &offer, qint64 copiedAt) {
    int pipefd[2];
    if (pipe2(pipefd, O_CLOEXEC) != 0)
      return;
    const auto id = offer.copyId;
    fd_ = pipefd[0];
    fcntl(fd_, F_SETFL, O_NONBLOCK);
    offer.receive(offer.mime, pipefd[1]);
    close(pipefd[1]);
    notifier_ =
        std::make_unique<QSocketNotifier>(fd_, QSocketNotifier::Read, this);
    connect(notifier_.get(), &QSocketNotifier::activated, this, [this, id, copiedAt] {
      char chunk[8192];
      for (;;) {
        auto count = read(fd_, chunk, sizeof(chunk));
        if (count > 0) {
          bytes_.append(chunk, count);
        } else if (count == 0) {
          auto text = QString::fromUtf8(bytes_);
          cancelRead();
          if (!text.isEmpty())
            copied_(text, id, copiedAt);
          return;
        } else if (errno == EINTR) {
          continue;
        } else {
          if (errno != EAGAIN && errno != EWOULDBLOCK)
            cancelRead();
          return;
        }
      }
    });
  }
  std::function<void(QString, QString, qint64)> copied_;
  std::unique_ptr<Device> device_;
  std::unique_ptr<QSocketNotifier> notifier_;
  int fd_ = -1;
  QByteArray bytes_;
};
