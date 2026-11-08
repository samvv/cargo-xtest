# Arch matches the glibc of the machine the tests are built on.
FROM archlinux:latest
RUN pacman -Sy --noconfirm borg && pacman -Scc --noconfirm
