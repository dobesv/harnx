# Custom Theme

Harnx supports custom themes via `.tmTheme` files. You can use a custom dark theme and a custom light theme to highlight response text and code blocks.

## Setup

1. Download a `.tmTheme` file.
2. Place it in the Harnx config directory.
3. Name it `dark.tmTheme` for the dark theme or `light.tmTheme` for the light theme.
4. Harnx will automatically load the theme on startup.

Navigate to the config directory:

```sh
cd "$(dirname "$(harnx --info | grep config_file | awk '{print $2}')")"
```

Download a dark theme:

```sh
wget -O dark.tmTheme <theme-url>
```

Download a light theme:

```sh
wget -O light.tmTheme <theme-url>
```

## Available Themes

### 1337-Scheme

```sh
wget -O dark.tmTheme https://raw.githubusercontent.com/MarkMichos/1337-Scheme/master/1337-Scheme.tmTheme
```

### Coldark

Dark:

```sh
wget -O dark.tmTheme https://raw.githubusercontent.com/ArmandPhilworthy/coldark-tmTheme/master/Coldark-Dark.tmTheme
```

Light:

```sh
wget -O light.tmTheme https://raw.githubusercontent.com/ArmandPhilworthy/coldark-tmTheme/master/Coldark-Cold.tmTheme
```

### Dracula

```sh
wget -O dark.tmTheme https://raw.githubusercontent.com/dracula/sublime/master/Dracula.tmTheme
```

### Github

```sh
wget -O light.tmTheme https://raw.githubusercontent.com/primer/github-textmate-theme/main/GitHub.tmTheme
```

### gruvbox

Dark:

```sh
wget -O dark.tmTheme https://raw.githubusercontent.com/peaceant/gruvbox/master/gruvbox%20(Dark)%20(Hard).tmTheme
```

Light:

```sh
wget -O light.tmTheme https://raw.githubusercontent.com/peaceant/gruvbox/master/gruvbox%20(Light)%20(Hard).tmTheme
```

### OneHalf

Dark:

```sh
wget -O dark.tmTheme https://raw.githubusercontent.com/sonph/onehalf/master/sublimetext/OneHalfDark.tmTheme
```

Light:

```sh
wget -O light.tmTheme https://raw.githubusercontent.com/sonph/onehalf/master/sublimetext/OneHalfLight.tmTheme
```

### Solarized

Dark:

```sh
wget -O dark.tmTheme https://raw.githubusercontent.com/braver/Solarized/master/Solarized%20(dark).tmTheme
```

Light:

```sh
wget -O light.tmTheme https://raw.githubusercontent.com/braver/Solarized/master/Solarized%20(light).tmTheme
```

### Sublime Snazzy

```sh
wget -O dark.tmTheme https://raw.githubusercontent.com/greggb/sublime-snazzy/master/Sublime%20Snazzy.tmTheme
```

### TwoDark

```sh
wget -O dark.tmTheme https://raw.githubusercontent.com/nicklayb/TwoDark/master/TwoDark.tmTheme
```

### Visual Studio Dark+

```sh
wget -O dark.tmTheme https://raw.githubusercontent.com/nicklayb/VSDarkPlus-tmTheme/master/VSDarkPlus.tmTheme
```

### Zenburn

```sh
wget -O dark.tmTheme https://raw.githubusercontent.com/colinta/zenburn/master/zenburn.tmTheme
```
