# SPDX-License-Identifier: MPL-2.0
#
# Built from a source tarball of the repository (git archive), with the crates fetched by Cargo:
# the CI's container has the network. The tarball's top directory is %{name}-%{version}.

Name:           qemu-dbus-webrtc
Version:        0.2.0
Release:        1%{?dist}
Summary:        WebRTC console for QEMU virtual machines
License:        MPL-2.0
URL:            https://github.com/ameleo/qemu-dbus-webrtc
Source0:        %{name}-%{version}.tar.gz
# The account, also in the tarball: a source of its own, which %%pre takes in when built.
Source1:        qemu-dbus-webrtc.sysusers

ExclusiveArch:  x86_64

BuildRequires:  cargo >= 1.89
BuildRequires:  rust >= 1.89
BuildRequires:  clang-devel
BuildRequires:  libva-devel
BuildRequires:  libvpx-devel
BuildRequires:  opus-devel
BuildRequires:  pkgconfig
BuildRequires:  systemd-rpm-macros

# The VA driver of the GPU encodes the video: Intel's (iHD, from RPM Fusion) or Mesa's.
Recommends:     (intel-media-driver or mesa-va-drivers)
# libvirt's VMs are found through libvirt-dbus.
Recommends:     libvirt-dbus
%{?sysusers_requires_compat}

# Rust's release profile: no separate debug information to extract.
%global debug_package %{nil}

%description
qemu-dbus-webrtc serves the console of QEMU virtual machines (screens, sound, keyboard and
pointer, clipboard, drives, printers, cameras) over WebRTC, from QEMU's D-Bus display, with the
video encoded on the GPU (AV1, HEVC, VP9 through VA-API) or on the CPU (VP9).

%prep
%autosetup -p1

%build
# The commit the tarball comes from, logged by the server at startup.
%{?commit:export QDW_COMMIT=%{commit}}
cargo build --release --locked -p qemu-dbus-webrtc

%install
install -D -m 0755 target/release/qemu-dbus-webrtc %{buildroot}%{_bindir}/qemu-dbus-webrtc
install -D -m 0644 packaging/qemu-dbus-webrtc.service %{buildroot}%{_unitdir}/qemu-dbus-webrtc.service
install -D -m 0644 %{SOURCE1} %{buildroot}%{_sysusersdir}/qemu-dbus-webrtc.conf
install -D -m 0640 config.example.toml %{buildroot}%{_sysconfdir}/qemu-dbus-webrtc/config.toml
install -d -m 0700 %{buildroot}%{_sysconfdir}/qemu-dbus-webrtc/tls

%pre
%sysusers_create_compat %{SOURCE1}

%post
%systemd_post qemu-dbus-webrtc.service

%preun
%systemd_preun qemu-dbus-webrtc.service

%postun
%systemd_postun_with_restart qemu-dbus-webrtc.service

%files
%license LICENSE
%doc README.md ARCHITECTURE.md config.example.toml
%{_bindir}/qemu-dbus-webrtc
%{_unitdir}/qemu-dbus-webrtc.service
%{_sysusersdir}/qemu-dbus-webrtc.conf
%dir %attr(0755,root,root) %{_sysconfdir}/qemu-dbus-webrtc
# Read by the service's account; the certificate's directory, by root alone (systemd hands its
# files to the service).
%config(noreplace) %attr(0640,root,qemu-dbus-webrtc) %{_sysconfdir}/qemu-dbus-webrtc/config.toml
%dir %attr(0700,root,root) %{_sysconfdir}/qemu-dbus-webrtc/tls

%changelog
* Mon Nov 16 2026 Mickaël Bourneuf <mickael.bourneuf@ameleo.fr> - 0.2.0-1
- Web page redone: toolbar, connection details, reconnection, English and French
- All monitors in Chrome and Edge, file drop, every key to the guest in full screen
- Accounts, each with the VMs it may open
- Clients outside the host's network: STUN (Google's by default), or forwarded UDP ports
- Plain virtio-vga with blob scanouts, imported by the GPU and kept between flips
- OpenGL framebuffers encoded the right way up; missed screen layouts asked again

* Sun Nov 15 2026 Mickaël Bourneuf <mickael.bourneuf@ameleo.fr> - 0.1.3-1
- Keep the client files the guest locked readable after the guest copies something else
- Documentation: why the project exists, and links to the guest agents and their protocol

* Sat Nov 14 2026 Mickaël Bourneuf <mickael.bourneuf@ameleo.fr> - 0.1.2-1
- Encode in variable bitrate on the GPU: no more filler data on still screens nor frame rate
  collapse to 4 fps
- Take the guest agent's D-Bus contract from ameleo-display-protocol
- Log identical scanouts at debug level, and the version and commit at startup

* Sat Nov 14 2026 Mickaël Bourneuf <mickael.bourneuf@ameleo.fr> - 0.1.1-1
- On a narrow link, drop frames while the encoder is ahead of its bitrate, rather than let
  the sender's queue delay the picture and the sound
- Aim the video at 85 % of what the bandwidth estimate leaves after the sound
- Take a bandwidth estimate that came within 500 ms of the last bitrate change
- Log the estimate, the bitrate asked of each screen and the frames dropped

* Sat Nov 14 2026 Mickaël Bourneuf <mickael.bourneuf@ameleo.fr> - 0.1.0-1
- First release
