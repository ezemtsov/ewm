# Boots an EWM session under greetd and runs tests/ewm-session-tests.el in it.
# The compositor needs the host GPU, run the driver outside the sandbox with
# KVM and a render node.
{
  stdenv,
  testers,
  qemu_kvm,
  mesa,
  symlinkJoin,
  makeWrapper,
  writeShellApplication,
  writeText,
  withSkia,
}:
let
  # Mesa from this nixpkgs, the host GL stack may link another glibc.
  qemu-virgl = symlinkJoin {
    name = "qemu-virgl";
    paths = [ qemu_kvm ];
    nativeBuildInputs = [ makeWrapper ];
    postBuild = ''
      wrapProgram $out/bin/qemu-system-${stdenv.hostPlatform.qemuArch} \
        --set __EGL_VENDOR_LIBRARY_DIRS ${mesa}/share/glvnd/egl_vendor.d \
        --set LIBGL_DRIVERS_PATH ${mesa}/lib/dri \
        --set GBM_BACKENDS_PATH ${mesa}/lib/gbm
    '';
  };

  test = testers.runNixOSTest {
    name = "ewm-session-${if withSkia then "pwayl" else "pgtk"}";
    qemu.package = qemu-virgl;

    nodes.machine =
      { config, pkgs, ... }:
      {
        imports = [ ./service.nix ];
        programs.ewm = {
          enable = true;
          inherit withSkia;
        };
        services.greetd = {
          enable = true;
          settings.default_session = {
            command = "ewm-session";
            user = "alice";
          };
        };
        users.users.alice = {
          isNormalUser = true;
          uid = 1000;
          password = "alice";
        };
        systemd.tmpfiles.rules =
          let
            # The forced head offers no 1280x800 mode, hence the custom one.
            init = writeText "init.el" ''
              ;;; -*- lexical-binding: t -*-
              (setq ewm-output-config '(("Virtual-1" :scale 1.5)
                                        ("Virtual-2" :width 1280 :height 800 :refresh 60 :custom t :scale 1.0)))
            '';
          in
          [
            "d /home/alice/.config 0755 alice users -"
            "d /home/alice/.config/emacs 0755 alice users -"
            "L+ /home/alice/.config/emacs/init.el - - - - ${init}"
          ];
        environment.systemPackages = [
          config.programs.ewm.emacsPackage
          pkgs.foot
          pkgs.grim
          pkgs.swaylock
        ];
        # QEMU enables only the first head.
        boot.kernelParams = [ "video=Virtual-2:1280x800e" ];
        virtualisation = {
          memorySize = 4096;
          cores = 4;
          qemu.options = [
            "-vga none"
            "-device virtio-vga-gl,max_outputs=2"
            "-display egl-headless"
          ];
        };
      };

    testScript = ''
      import shlex

      SOCKET = "/run/user/1000/emacs/server"

      def eval_cmd(form):
          """FORM evaluated in the session as alice."""
          client = f"emacsclient -s {SOCKET} --eval {shlex.quote(form)} 2>&1"
          return f"su - alice -c {shlex.quote(client)}"

      def check(form):
          """Evaluate FORM, a check that returns its PASS line or signals."""
          print(machine.succeed(eval_cmd(form)))

      READY = f"{eval_cmd('ewm--compositor-ready')} | grep -qx t"

      with subtest("greetd logs alice into an EWM session"):
          machine.wait_until_succeeds(READY, timeout=180)
          machine.succeed(eval_cmd('(progn (load "${../tests/ewm-session-tests.el}") (setq ewm-session-tests-dir "/tmp/session"))'))

      with subtest("both outputs come up with their configured scales"):
          check('(ewm-session-tests-outputs \'(("Virtual-1" . 1.5) ("Virtual-2" . 1.0)))')
          check("(ewm-session-tests-setup)")

      with subtest("keyboard input reaches the focused frame"):
          machine.send_chars("typed in the vm")
          print(machine.wait_until_succeeds(eval_cmd('(ewm-session-tests-typed "typed in the vm")'), timeout=30))

      with subtest("a child frame survives"):
          check("(ewm-session-tests-child-frame)")

      with subtest("an app goes fullscreen over its output and back"):
          check("(ewm-session-tests-fullscreen)")

      with subtest("one app surface shows in two windows at once"):
          check("(ewm-session-tests-views)")

      with subtest("the focused frame moves to the other output and back"):
          check("(ewm-session-tests-move)")

      with subtest("the session locks, swallows input and unlocks with the password"):
          check("(ewm-session-tests-lock)")
          machine.send_chars("alice\n")
          check('(ewm-session-tests-unlocked "alice")')

      check("(ewm-session-tests-teardown)")
      machine.copy_from_machine("/tmp/session")

      with subtest("logout ends the session cleanly and greetd logs in again"):
          # greetd logs in again at once, watch the pid.
          pid = machine.succeed(eval_cmd("(emacs-pid)")).strip()
          machine.execute(eval_cmd("(kill-emacs)"))
          machine.wait_until_fails(f"kill -0 {pid}", timeout=60)
          machine.fail("journalctl -b --no-pager | grep -qE 'dumped core|ewm.service.*(Failed with result|code=(killed|dumped))'")
          machine.wait_until_succeeds(READY, timeout=180)
    '';
  };
in
# Without a display variable the driver passes -nographic. It keys its sockets
# and machine state by fixed names under XDG_RUNTIME_DIR or TMPDIR, a directory
# per run keeps two drivers apart.
writeShellApplication {
  name = "nixos-test-driver";
  text = ''
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    env -u XDG_RUNTIME_DIR TMPDIR="$tmp" WAYLAND_DISPLAY="''${WAYLAND_DISPLAY-egl-headless}" \
      ${test.driver}/bin/nixos-test-driver "$@"
  '';
}
