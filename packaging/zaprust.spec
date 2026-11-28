Name:           zaprust
Version:        @VERSION@
Release:        1
Summary:        Zaprust, a fast native WhatsApp client
License:        MIT
BuildArch:      @ARCH@
URL:            https://github.com/fanyzp/zaprust

%description
A native WhatsApp desktop client written in Rust, every feature built in.

%install
mkdir -p %{buildroot}/usr/bin
mkdir -p %{buildroot}/usr/share/applications
mkdir -p %{buildroot}/usr/share/icons/hicolor/512x512/apps
cp @STAGE@/usr/bin/zaprust %{buildroot}/usr/bin/zaprust
cp @STAGE@/usr/share/applications/zaprust.desktop %{buildroot}/usr/share/applications/zaprust.desktop
cp @STAGE@/usr/share/icons/hicolor/512x512/apps/zaprust.png %{buildroot}/usr/share/icons/hicolor/512x512/apps/zaprust.png

%files
/usr/bin/zaprust
/usr/share/applications/zaprust.desktop
/usr/share/icons/hicolor/512x512/apps/zaprust.png
