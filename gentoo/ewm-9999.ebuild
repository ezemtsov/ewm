# Copyright 1999-2026 Gentoo Authors
# Distributed under the terms of the GNU General Public License v2

EAPI=8

RUST_MIN_VER="1.87.0"

inherit cargo elisp-common git-r3 systemd

DESCRIPTION="Modern, fast Linux desktop environment based on Emacs"
HOMEPAGE="https://codeberg.org/ezemtsov/ewm"
EGIT_REPO_URI="https://codeberg.org/ezemtsov/ewm.git"
S="${WORKDIR}/${P}/compositor"

LICENSE="GPL-3+"
# Dependent crate licenses
LICENSE+=" Apache-2.0 Apache-2.0-with-LLVM-exceptions BSD ISC MIT Unicode-3.0"
SLOT="0"
IUSE="+screencast systemd"

DEPEND="
	dev-libs/glib:2
	dev-libs/libinput
	>=dev-libs/wayland-1.23
	media-libs/libdisplay-info
	media-libs/libglvnd
	media-libs/mesa
	sys-auth/seatd
	virtual/libudev
	x11-libs/libdrm
	x11-libs/libxkbcommon
	screencast? ( >=media-video/pipewire-0.3.33 )
"
RDEPEND="${DEPEND}
	>=app-editors/emacs-29.1:*[dynamic-loading,gui,gtk,-X]
	sys-apps/dbus
	screencast? (
		sys-apps/xdg-desktop-portal-gnome
		sys-apps/xdg-desktop-portal-gtk
	)
"
BDEPEND="
	>=app-editors/emacs-29.1:*
	virtual/pkgconfig
	screencast? ( llvm-core/clang )
"

src_unpack() {
	git-r3_src_unpack
	cargo_live_src_unpack
}

src_configure() {
	local myfeatures=( $(usev screencast) )
	cargo_src_configure --frozen
}

src_compile() {
	cargo_src_compile
	BYTECOMPFLAGS="-L ../lisp" elisp-compile ../lisp/*.el
}

src_install() {
	cp "$(cargo_target_dir)"/libewm_core.so "${T}"/ewm-core.so || die
	elisp-modules-install "${PN}" "${T}"/ewm-core.so
	elisp-install "${PN}" ../lisp/*.el ../lisp/*.elc
	elisp-make-site-file "50${PN}-gentoo.el" "${PN}" \
		"(add-to-list 'load-path \"@EMACSMODULES@\")"

	newbin - ewm-launch <<-EOS
	#!/bin/sh
	exec emacs --fg-daemon -l ewm -f ewm-start-module "\$@"
	EOS

	sed "s:^Exec=.*:Exec=$(usex systemd ewm-session ewm-launch):" \
		../resources/ewm.desktop > "${T}"/ewm.desktop || die
	insinto /usr/share/wayland-sessions
	doins "${T}"/ewm.desktop
	if use systemd; then
		dobin ../resources/ewm-session
		systemd_douserunit ../resources/ewm.service ../resources/ewm-shutdown.target
	fi

	if use screencast; then
		insinto /usr/share/xdg-desktop-portal
		doins ../resources/ewm-portals.conf
	fi
}

pkg_postinst() { elisp-site-regen; }
pkg_postrm() { elisp-site-regen; }
