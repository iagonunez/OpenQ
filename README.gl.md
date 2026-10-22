<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/img/logo-dark.svg">
    <img src="docs/img/logo-light.svg" alt="OpenQ" width="300">
  </picture>
</p>

<h3 align="center">Control de espectáculos en directo para son, vídeo e escena</h3>

<p align="center">
  <a href="README.md">English</a> · <a href="README.es.md">Español</a> · <b>Galego</b>
  &nbsp;|&nbsp; 🌐 <a href="https://iagonunez.github.io/OpenQ/gl/">Web</a>
  &nbsp;|&nbsp; 📖 <a href="https://iagonunez.github.io/OpenQ/manual/index.gl.html">Manual de usuario</a>
</p>

---

## 🎭 Acerca de

OpenQ leva unha obra de teatro, un concerto ou un evento a partir dunha lista de cues: cada pulsación
de **GO** dispara a seguinte, xa sexa música, un efecto de son, unha película nun proxector, un
rótulo nunha pantalla, un fundido ou unha mensaxe á mesa de luces. É unha aplicación de código
dispoñible escrita en Rust con [egui](https://github.com/emilk/egui), e o seu uso é de balde, tamén
en producións comerciais.

<p align="center">
  <img src="docs/img/screenshot.gl.webp" alt="OpenQ levando un espectáculo: o botón GO, a cue en espera, unha lista con cues de audio, vídeo, texto, grupo, fundido e rede, e as cues que están a soar" width="900">
</p>

<p align="center">
  <a href="LICENSE.md"><img src="https://img.shields.io/badge/Licenza-Apache%202.0-3cc864?style=for-the-badge" alt="Licenza: Apache 2.0"></a>
</p>

## 📦 Instalación

OpenQ instálase con **[OQup](https://github.com/iagonunez/OQup)**, un instalador ao estilo de
rustup. Descarga a versión máis recente de OpenQ para o teu sistema coas bibliotecas FFmpeg só LGPL
que usa (en Linux instala ademais as bibliotecas de son e dispositivos que falten cos paquetes da
túa distribución, preguntando antes), engade o visualizador 3D OQVis para o espazo de iluminación e
instala a aplicación coa súa entrada no menú. `oqup update` instala unha versión nova cando sae,
`oqup versions` amósaas e `oqup uninstall` elimínaa. `oqup install --build-source` compila OpenQ no
teu ordenador (instalando Rust e as dependencias de compilación se faltan).
`oqup install --no-lighting` instala só os cues, sen o espazo de iluminación nin OQVis
(`--lighting` volve engadilo).

### 🐧 Linux e 🍎 macOS

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/iagonunez/OQup/releases/latest/download/install.sh | sh
```

Os binarios de Linux precisan Debian 12, Ubuntu 23.04, Fedora 37 ou posterior (glibc 2.36). En sistemas máis
antigos usa `oqup install --build-source --no-default-features --features video,lighting` (son por ALSA: o seu
PipeWire é demasiado antigo para o soporte de PipeWire de OpenQ).

### 🪟 Windows 10/11 (64 bits), en PowerShell

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/iagonunez/OQup/releases/latest/download/install.ps1 | iex"
```

A aplicación instálase en `~/Applications/OpenQ.app` (macOS, asinada ad hoc ou co teu Developer ID:
`--sign`), `%LOCALAPPDATA%\Programs\OpenQ` (Windows) ou `~/.local/share/oqup/app` cunha ligazón en
`~/.local/bin/openq` (Linux). O README de OQup enumera cada descarga, a súa orixe e a súa licenza.

### 🛠️ Desde o código, á man (Linux; o que fai alí `oqup install --build-source`)

```sh
# Debian/Ubuntu
sudo apt install build-essential clang pkg-config libasound2-dev libpipewire-0.3-dev libudev-dev \
  libavcodec-dev libavformat-dev libavutil-dev libavdevice-dev libavfilter-dev \
  libswscale-dev libswresample-dev
# Fedora:  sudo dnf install clang pkgconf alsa-lib-devel pipewire-devel systemd-devel ffmpeg-free-devel
# Arch:    sudo pacman -S --needed base-devel clang pkgconf alsa-lib libpipewire ffmpeg
curl https://sh.rustup.rs -sSf | sh          # Rust, se non o tes
git clone https://github.com/iagonunez/OpenQ && cd OpenQ
cargo build --release && ./target/release/openq
```

> [!NOTE]
> O audio pasa por PipeWire cando está en marcha (cada saída e entrada aparece polo seu nome), e
> por ALSA no caso contrario. Sen as cabeceiras de PipeWire, compila con
> `--no-default-features --features video` (só ALSA).

## ✨ Funcións

| | Función | Que fai |
|---|---|---|
| 🎬 | **24 tipos de cue** | Audio, Vídeo, Texto, Cámara, Micro, Grupo, Espera, Memo, Fundido, Iniciar, Parar, Pausa, Ir a, Devamp, Rede (OSC ou texto, UDP ou TCP), MIDI, Ficheiro MIDI, Timecode, Luz, e Armar, Desarmar, Restablecer, Cargar e Destino para cambiar outras cues durante a función. |
| 🗂️ | **Listas de cues** | Varias listas por espectáculo en lapelas (a secuencia principal, unha lista de efectos para disparar á man…); as cues seguen a soar ao cambiar. |
| ⏱️ | **Tempos** | Pre-espera, post-espera, autocontinuar e autoseguir. As cues encadeadas arrancan na mostra exacta, por longa que sexa a cadea. |
| 📁 | **Grupos** | Entrar no grupo, lista de reprodución, lanzalo todo á vez ou un fillo ao chou. |
| 🔒 | **Modo edición e modo función** | Monta o espectáculo en modo edición; o modo función bloquea a lista e amosa filas grandes e lexibles para a cabina. |
| 🔊 | **Reprodución de audio** | Recortes, bucles, entrada e saída en fundido, varispeed e fragmentos que se repiten ata que unha cue Devamp os libera. |
| 🎚️ | **Mesturador e enrutado** | Nivel, panorama, faders de saída e unha matriz de puntos de cruzamento por cue (ata 64 saídas), con dB en cada punto e puntos agrupados. Máis dispositivos de saída como patches de audio. |
| 📏 | **Sonoridade** | Mide en LUFS a sonoridade de cada ficheiro de son; nivela unha cue ou toda unha selección ao obxectivo do espectáculo. |
| 📈 | **Ecualizador** | Ata 8 bandas por cue (filtros paso alto e baixo, shelves, picos), cunha curva de resposta que se arrastra. |
| 🎛️ | **Efectos de audio** | Cadeas de delay, reverb, compresor, limitador, porta, distorsión, chorus e bitcrusher que se editan en directo. |
| 🔈 | **Audio por obxectos** | Coloca cada canle nunha sala de altofalantes (de estéreo a 7.1 ou a túa propia disposición); as cues de fundido moven os sons pola sala. |
| 🎤 | **Entradas en directo** | As cues de micro reproducen un micrófono ou unha entrada de liña co mesmo mesturador, ecualizador e efectos. |
| 🎞️ | **Vídeo e imaxes** | Todo o que FFmpeg pode reproducir, ademais de PNG, JPEG, WebP, GIF e SVG nítidos, con recortes, bucles e enrutado da banda sonora. |
| 🔤 | **Cues de texto** | Títulos e sobretítulos con calquera fonte instalada, con contorno, sombra e caixa de fondo; todas as linguas e emoji. |
| 📐 | **Xeometría e animación** | Posición, escala, rotación, recorte e opacidade, ademais de animación por fotogramas clave con curvas de suavizado, calculada en directo na GPU. |
| 🌈 | **Efectos de vídeo** | Controis de cor, ton, colorear, desenfoque, pixelado, viñeta, croma e máis, debuxados en directo pola tarxeta gráfica. |
| 🧩 | **Modos de fusión** | Sumar, Trama, Multiplicar, Aclarar, Escurecer e Restar por capa. |
| 📽️ | **Escenarios e mapping** | Tantas saídas como queiras, a pantalla completa en calquera pantalla, con corrección de trapecio por esquinas e un patrón de axuste. |
| 🌅 | **Edge blending** | Reparte unha imaxe entre varios proxectores con solapamentos suaves, corrección de gamma e elevación do negro; as máscaras ocultan partes de capas e saídas. |
| 📷 | **Cues de cámara** | Amosa unha cámara ou unha capturadora en directo como capa no escenario. |
| 🌗 | **Fundidos** | Funde nivel, faders de saída, efectos de audio e vídeo, opacidade, xeometría e posicións de obxectos, con curvas predefinidas ou propias. |
| 🕰️ | **Código de tempo** | Cada lista pode seguir LTC ou MTC e disparar cues no seu momento; as cues Timecode envían LTC ou MTC. |
| ⌨️ | **Disparadores** | Dispara calquera cue coa súa propia tecla, unha mensaxe MIDI ou unha hora do día, ou dende un cart: unha grella de botóns. |
| 🎹 | **Saída MIDI** | As cues MIDI envían notas, cambios de control e de programa, MIDI Show Control e SysEx. |
| 📦 | **Empaquetar** | Copia un espectáculo e todos os seus medios nun cartafol para movelo ou arquivalo. |
| 📡 | **OSC** | Controla OpenQ desde outros equipos por OSC e envía OSC a mesas de luces coas cues de rede. |
| 📱 | **Dispositivos remotos** | Emparella un portátil ou unha tableta para seguir o espectáculo, levalo ou editalo, cunha conexión cifrada. |
| 🛟 | **Seguridade ante fallos** | Autogardado e recuperación, gardado atómico, dispositivos de audio que se reconectan sós e un rexistro de erros. |
| ↩️ | **Desfacer e refacer** | Ata 200 pasos. |
| 💻 | **Multiplataforma** | Linux, macOS e Windows. |

## 📖 Manual de usuario

Todo o anterior explícase paso a paso, con imaxes, no **manual de usuario**, que vén coa aplicación:
escolle **Axuda → Manual de usuario** ou preme **F1** e ábrese no teu
navegador. Funciona sen conexión e sempre corresponde á túa versión de OpenQ. Como a propia aplicación (a lingua escóllese a primeira vez que se abre ou en
**Axustes → Xeral**), está en galego, español e inglés, e tamén podes lelo [en liña](https://iagonunez.github.io/OpenQ/manual/index.gl.html).

## 🎨 Aspecto e créditos

A interface usa un tema escuro (`src/ui/theme.rs`) con fontes incluídas en `assets/fonts`:

- Iconas [Material Symbols Rounded](https://fonts.google.com/icons) (Apache-2.0)
- [Inter](https://fonts.google.com/specimen/Inter) para o texto,
  [JetBrains Mono](https://fonts.google.com/specimen/JetBrains+Mono) para os tempos e
  [Barlow Condensed](https://fonts.google.com/specimen/Barlow+Condensed) para o título de OpenQ
  (SIL OFL 1.1)

Só inclúen os glifos que se usan. Para engadir unha icona, pon o seu nome en `tools/icons.txt` e
volve xeralas con `python3 -I tools/build_fonts.py <cartafol coas fontes orixinais>`, que tamén
rexenera `src/ui/icons.rs`.

## ⚖️ Licenza

Copyright 2026 Iago Núñez. OpenQ distribúese baixo a **Apache License 2.0** ([LICENSE.md](LICENSE.md)). Este resumo é orientativo; o texto en inglés é o que vale:

- ✅ **Permitido:** calquera uso, tamén en producións comerciais. Podes facer funcións, concertos,
  eventos e instalacións de pago, e cobrar por programalos ou operalos. Podes copiar, modificar e
  compartir OpenQ, conservando o aviso de copyright, `LICENSE.md` e `NOTICE.md` en cada copia.
- ✅ **Tamén permitido:** vender copias ou versións modificadas de OpenQ, e servizos de
  aloxamento, soporte ou consultoría baseados nel. Os ficheiros modificados deben indicalo.

As fontes, crates e bibliotecas do sistema de terceiros conservan as súas propias licenzas. Están en
[NOTICE.md](NOTICE.md) e [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md), e tamén na aplicación
en **Axuda → Acerca de OpenQ e licenzas**. Dúas cousas a ter en conta ao distribuír binarios:

- Inclúe con eles `LICENSE.md`, `NOTICE.md`, `THIRD_PARTY_LICENSES.md` e `assets/fonts/*.txt`.
  A aplicación tamén contén estes textos.
- Compila cun **FFmpeg só LGPL** ou usa `--no-default-features`. Unha compilación GPL de FFmpeg
  (`--enable-gpl`, como a distribúen moitas distribucións de Linux) deixa un binario distribuído
  enteiro baixo a GPL 3, con todo o seu código fonte.

Despois de cambiar dependencias, executa `python3 -I tools/third_party_licenses.py`.

OpenQ é de código aberto: a Apache License 2.0 está aprobada pola Open Source Initiative.
