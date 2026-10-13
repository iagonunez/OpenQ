<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/img/logo-dark.svg">
    <img src="docs/img/logo-light.svg" alt="OpenQ" width="300">
  </picture>
</p>

<h3 align="center">Control de espectáculos en directo para sonido, vídeo y escena</h3>

<p align="center">
  <a href="README.md">English</a> · <b>Español</b> · <a href="README.gl.md">Galego</a>
  &nbsp;|&nbsp; 🌐 <a href="https://iagonunez.github.io/OpenQ/es/">Web</a>
  &nbsp;|&nbsp; 📖 <a href="https://iagonunez.github.io/OpenQ/manual/index.es.html">Manual de usuario</a>
</p>

---

## 🎭 Acerca de

OpenQ lleva una obra de teatro, un concierto o un evento a partir de una lista de cues: cada pulsación
de **GO** dispara la siguiente, ya sea música, un efecto de sonido, una película en un proyector, un
rótulo en una pantalla, un fundido o un mensaje a la mesa de luces. Es una aplicación de código
disponible escrita en Rust con [egui](https://github.com/emilk/egui), y su uso es gratuito, también
en producciones comerciales.

<p align="center">
  <img src="docs/img/screenshot.webp" alt="OpenQ llevando un espectáculo: el botón GO, la cue en espera, una lista con cues de audio, vídeo, texto, grupo, fundido y red, y las cues que están sonando" width="900">
</p>

<p align="center">
  <a href="LICENSE.md"><img src="https://img.shields.io/badge/Licencia-Apache%202.0-3cc864?style=for-the-badge" alt="Licencia: Apache 2.0"></a>
</p>

## 📦 Instalación

OpenQ se compila desde el código fuente en tu ordenador con **[openqup](https://github.com/iagonunez/openqup)**,
un instalador al estilo de rustup. Instala Rust y las dependencias de compilación si faltan (en
Windows y macOS un FFmpeg solo LGPL, en Linux los paquetes de tu distribución, preguntando antes),
compila la versión etiquetada más reciente e instala la aplicación con su entrada en el menú.
`openqup update` vuelve a compilar cuando sale una versión nueva; `openqup uninstall` la elimina.

### 🐧 Linux y 🍎 macOS

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/iagonunez/openqup/releases/latest/download/install.sh | sh
```

### 🪟 Windows 10/11 (64 bits), en PowerShell

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/iagonunez/openqup/releases/latest/download/install.ps1 | iex"
```

La aplicación se instala en `~/Applications/OpenQ.app` (macOS, firmada ad hoc o con tu Developer ID:
`--sign`), `%LOCALAPPDATA%\Programs\OpenQ` (Windows) o `~/.local/bin/openq` (Linux). El README de
openqup enumera cada descarga, su origen y su licencia.

### 🛠️ A mano (Linux; es lo que hace openqup allí)

```sh
# Debian/Ubuntu
sudo apt install build-essential clang pkg-config libasound2-dev libpipewire-0.3-dev libudev-dev \
  libavcodec-dev libavformat-dev libavutil-dev libavdevice-dev libavfilter-dev \
  libswscale-dev libswresample-dev
# Fedora:  sudo dnf install clang pkgconf alsa-lib-devel pipewire-devel systemd-devel ffmpeg-free-devel
# Arch:    sudo pacman -S --needed base-devel clang pkgconf alsa-lib libpipewire ffmpeg
curl https://sh.rustup.rs -sSf | sh          # Rust, si no lo tienes
git clone https://github.com/iagonunez/OpenQ && cd OpenQ
cargo build --release && ./target/release/openq
```

> [!NOTE]
> El audio pasa por PipeWire cuando está en marcha (cada salida y entrada aparece por su nombre), y
> por ALSA en caso contrario. Sin las cabeceras de PipeWire, compila con
> `--no-default-features --features video` (solo ALSA).

## ✨ Funciones

| | Función | Qué hace |
|---|---|---|
| 🎬 | **15 tipos de cue** | Audio, Vídeo, Texto, Cámara, Micro, Grupo, Espera, Memo, Fundido, Iniciar, Parar, Pausa, Ir a, Devamp y Red (OSC). |
| 🗂️ | **Listas de cues** | Varias listas por espectáculo en pestañas (la secuencia principal, una lista de efectos para disparar a mano…); las cues siguen sonando al cambiar. |
| ⏱️ | **Tiempos** | Pre-espera, post-espera, autocontinuar y autoseguir. Las cues encadenadas arrancan en la muestra exacta, por larga que sea la cadena. |
| 📁 | **Grupos** | Entrar en el grupo, lista de reproducción, lanzarlo todo a la vez o un hijo al azar. |
| 🔒 | **Modo edición y modo función** | Monta el espectáculo en modo edición; el modo función bloquea la lista y muestra filas grandes y legibles para la cabina. |
| 🔊 | **Reproducción de audio** | Recortes, bucles, entrada y salida en fundido, varispeed y fragmentos que se repiten hasta que una cue Devamp los libera. |
| 🎚️ | **Mezclador y enrutado** | Nivel, panorama, faders de salida y una matriz de puntos de cruce por cue, con dB en cada punto. |
| 📈 | **Ecualizador** | Hasta 8 bandas por cue (filtros paso alto y bajo, shelves, picos), con una curva de respuesta que se arrastra. |
| 🎛️ | **Efectos de audio** | Cadenas de delay, reverb, compresor, limitador, puerta, distorsión, chorus y bitcrusher que se editan en directo. |
| 🔈 | **Audio por objetos** | Coloca cada canal en una sala de altavoces (de estéreo a 7.1 o tu propia disposición); las cues de fundido mueven los sonidos por la sala. |
| 🎤 | **Entradas en directo** | Las cues de micro reproducen un micrófono o una entrada de línea con el mismo mezclador, ecualizador y efectos. |
| 🎞️ | **Vídeo e imágenes** | Todo lo que FFmpeg puede reproducir, además de PNG, JPEG, WebP, GIF y SVG nítidos, con recortes, bucles y enrutado de la banda sonora. |
| 🔤 | **Cues de texto** | Títulos y sobretítulos con cualquier fuente instalada, con contorno, sombra y caja de fondo; todos los idiomas y emoji. |
| 📐 | **Geometría y animación** | Posición, escala, rotación, recorte y opacidad, además de animación por fotogramas clave con curvas de suavizado, calculada en directo en la GPU. |
| 🌈 | **Efectos de vídeo** | Controles de color, tono, colorear, desenfoque, pixelado, viñeta, croma y más, dibujados en directo por la tarjeta gráfica. |
| 🧩 | **Modos de fusión** | Sumar, Trama, Multiplicar, Aclarar, Oscurecer y Restar por capa. |
| 📽️ | **Escenarios y mapping** | Tantas salidas como quieras, a pantalla completa en cualquier pantalla, con corrección de trapecio por esquinas y un patrón de ajuste. |
| 🌅 | **Edge blending** | Reparte una imagen entre varios proyectores con solapes suaves y corrección de gamma. |
| 📷 | **Cues de cámara** | Muestra una cámara o una capturadora en directo como capa en el escenario. |
| 🌗 | **Fundidos** | Funde nivel, opacidad, geometría, efectos de vídeo y posiciones de objetos, con curvas predefinidas o propias. |
| 🕰️ | **Código de tiempo** | Cada lista puede seguir LTC o MTC y disparar cues en su momento. |
| ⌨️ | **Atajos** | Dispara cualquier cue directamente con su propia tecla. |
| 📡 | **OSC** | Controla OpenQ desde otros equipos por OSC y envía OSC a mesas de luces con las cues de red. |
| 📱 | **Dispositivos remotos** | Empareja un portátil o una tableta para seguir el espectáculo, llevarlo o editarlo, con conexión cifrada. |
| 🛟 | **Seguridad ante fallos** | Autoguardado y recuperación, guardado atómico, dispositivos de audio que se reconectan solos y un registro de errores. |
| ↩️ | **Deshacer y rehacer** | Hasta 200 pasos. |
| 💻 | **Multiplataforma** | Linux, macOS y Windows. |

## 📖 Manual de usuario

Todo lo anterior se explica paso a paso, con imágenes, en el **manual de usuario**, que viene con la
aplicación: elige **Help → User manual** o pulsa **F1** y se abre en
tu navegador. Funciona sin conexión y siempre corresponde a tu versión de OpenQ. Está en inglés,
español y gallego, y también puedes leerlo [en línea](https://iagonunez.github.io/OpenQ/manual/index.es.html).

## 🎨 Aspecto y créditos

La interfaz usa un tema oscuro (`src/ui/theme.rs`) con fuentes incluidas en `assets/fonts`:

- Iconos [Material Symbols Rounded](https://fonts.google.com/icons) (Apache-2.0)
- [Inter](https://fonts.google.com/specimen/Inter) para el texto,
  [JetBrains Mono](https://fonts.google.com/specimen/JetBrains+Mono) para los tiempos y
  [Barlow Condensed](https://fonts.google.com/specimen/Barlow+Condensed) para el título de OpenQ
  (SIL OFL 1.1)

Solo incluyen los glifos que se usan. Para añadir un icono, pon su nombre en `tools/icons.txt` y
vuelve a generarlas con `python3 -I tools/build_fonts.py <carpeta con las fuentes originales>`, que
también regenera `src/ui/icons.rs`.

## ⚖️ Licencia

Copyright 2026 Iago Núñez. OpenQ se distribuye bajo la **Apache License 2.0** ([LICENSE.md](LICENSE.md)). Este resumen es orientativo; el texto en inglés es el que vale:

- ✅ **Permitido:** cualquier uso, también en producciones comerciales. Puedes hacer funciones,
  conciertos, eventos e instalaciones de pago, y cobrar por programarlos u operarlos. Puedes copiar,
  modificar y compartir OpenQ, conservando el aviso de copyright, `LICENSE.md` y `NOTICE.md` en cada copia.
- ✅ **También permitido:** vender copias o versiones modificadas de OpenQ, y servicios de
  alojamiento, soporte o consultoría basados en él. Los archivos modificados deben indicarlo.

Las fuentes, crates y bibliotecas del sistema de terceros conservan sus propias licencias. Están en
[NOTICE.md](NOTICE.md) y [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md), y también en la
aplicación en **Help → About OpenQ and licenses**. Dos cosas a tener en cuenta al distribuir binarios:

- Incluye con ellos `LICENSE.md`, `NOTICE.md`, `THIRD_PARTY_LICENSES.md` y `assets/fonts/*.txt`.
  La aplicación también contiene estos textos.
- Compila con un **FFmpeg solo LGPL** o usa `--no-default-features`. Una compilación GPL de FFmpeg
  (`--enable-gpl`, como la distribuyen muchas distribuciones de Linux) deja un binario distribuido
  entero bajo la GPL 3, con todo su código fuente.

Después de cambiar dependencias, ejecuta `python3 -I tools/third_party_licenses.py`.

OpenQ es de código abierto: la Apache License 2.0 está aprobada por la Open Source Initiative.
