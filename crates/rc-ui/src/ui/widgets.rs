//! 可复用 UI 构件（XAML 模板 + FindName 装配）。
//!
//! 模板字符串集中在本模块，`XamlReader::Load` 解析后按名字填充内容并
//! 返回根元素；调用方再以 `FindName` 取出内部控件接线。避免手写
//! `createElement` 链，也避免整页 XAML 的重复。

use windows::core::{Interface, Result, HSTRING};
use windows::UI::Xaml::{
    Controls::Primitives::{ButtonBase, RangeBase},
    Controls::{
        Border, FontIcon, IconElement, MenuFlyoutItem, ProgressBar, TextBlock, ToolTipService,
    },
    FrameworkElement,
    Input::PointerEventHandler,
    Markup::XamlReader,
    Media::{Brush, SolidColorBrush},
    Shapes::Ellipse,
    UIElement, Visibility,
};

/// 卡片行平时的底色 / 描边，与设计令牌 `CardBackgroundBrush` / `CardBorderBrush` 一致；悬停时换成略亮的一对。
const CARD_FILL: &str = "#FFFFFF";
const CARD_STROKE: &str = "#EDF1F7";
const CARD_HOVER_FILL: &str = "#F5F8FF";
const CARD_HOVER_STROKE: &str = "#CFDDFF";

/// 装配一个设备行，返回行根元素；内容由调用方 `FindName` 填充。
///
/// 悬停时卡片底色微蓝、描边亮一点——列表行的手感都从这里来，所以装好就接上。
pub fn load_device_row() -> Result<FrameworkElement> {
    let row: FrameworkElement = XamlReader::Load(DEVICE_ROW_XAML_STR)?.cast()?;
    let card: Border = row.clone().cast()?;
    let pointer: UIElement = card.clone().cast()?;
    let hovered = card.clone();
    pointer.PointerEntered(PointerEventHandler::new(move |_, _| {
        hovered.SetBackground(&solid_brush(CARD_HOVER_FILL)?)?;
        hovered.SetBorderBrush(&solid_brush(CARD_HOVER_STROKE)?)
    }))?;
    let left = card;
    pointer.PointerExited(PointerEventHandler::new(move |_, _| {
        left.SetBackground(&solid_brush(CARD_FILL)?)?;
        left.SetBorderBrush(&solid_brush(CARD_STROKE)?)
    }))?;
    Ok(row)
}

/// 装配一个最近/收藏行。
pub fn load_recent_row() -> Result<FrameworkElement> {
    XamlReader::Load(RECENT_ROW_XAML_STR)?.cast()
}

/// 从已解析的行根元素中按名取菜单项。
pub fn find_menu_item(root: &FrameworkElement, name: &str) -> Result<MenuFlyoutItem> {
    root.FindName(name)?.cast()
}

/// 设备行模板。内部命名控件：
/// `RowName`/`RowCode`/`RowDot`/`RowState`/`RowLocal`/`RowLan`/
/// `RowConnect`/`RowMore`/`RowFavItem`/`RowRenameItem`/`RowUnbindItem`/`RowWolItem`。
pub const DEVICE_ROW_XAML_STR: &str = r##"<Border
    xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
    xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
    Background="{ThemeResource CardBackgroundBrush}"
    BorderBrush="{ThemeResource CardBorderBrush}" BorderThickness="1"
    CornerRadius="12" Padding="16,12">
  <Grid ColumnSpacing="12">
    <Grid.ColumnDefinitions>
      <ColumnDefinition Width="Auto"/>
      <ColumnDefinition Width="*"/>
      <ColumnDefinition Width="Auto"/>
    </Grid.ColumnDefinitions>
    <Border Width="44" Height="44" CornerRadius="12" Background="{ThemeResource AccentSoftBrush}">
      <FontIcon FontFamily="Segoe MDL2 Assets" Glyph="&#xE7F4;" FontSize="18" Foreground="{ThemeResource AccentStrongBrush}"
                HorizontalAlignment="Center" VerticalAlignment="Center"/>
    </Border>
    <StackPanel Grid.Column="1" Spacing="4" VerticalAlignment="Center">
      <StackPanel Orientation="Horizontal" Spacing="8">
        <TextBlock x:Name="RowName" FontSize="14" FontWeight="SemiBold" Foreground="{ThemeResource TextPrimaryBrush}" MaxWidth="240" TextTrimming="CharacterEllipsis"/>
        <Border x:Name="RowLocal" Background="{ThemeResource AccentSoftBrush}" CornerRadius="5" Padding="6,1" Visibility="Collapsed">
          <TextBlock Text="本机" FontSize="11" Foreground="{ThemeResource AccentStrongBrush}"/>
        </Border>
        <Border x:Name="RowLan" Background="#E7F8F0" CornerRadius="5" Padding="6,1" Visibility="Collapsed">
          <TextBlock Text="局域网" FontSize="11" Foreground="{ThemeResource SuccessBrush}"/>
        </Border>
      </StackPanel>
      <StackPanel Orientation="Horizontal" Spacing="6">
        <Ellipse x:Name="RowDot" Width="7" Height="7" Fill="{ThemeResource OfflineBrush}" VerticalAlignment="Center"/>
        <TextBlock x:Name="RowCode" FontFamily="Consolas" FontSize="12" Foreground="{ThemeResource TextTertiaryBrush}" VerticalAlignment="Center"/>
        <TextBlock x:Name="RowState" FontSize="12" Foreground="{ThemeResource TextTertiaryBrush}" VerticalAlignment="Center"/>
      </StackPanel>
    </StackPanel>
    <StackPanel Grid.Column="2" Orientation="Horizontal" Spacing="4" VerticalAlignment="Center">
      <Button x:Name="RowConnect" Content="连接" Style="{StaticResource AccentSoftButton}"/>
      <Button x:Name="RowMore" Style="{StaticResource LightGhostIconButton}" AutomationProperties.Name="更多操作" ToolTipService.ToolTip="更多">
        <FontIcon FontFamily="Segoe MDL2 Assets" Glyph="&#xE712;" FontSize="14"/>
        <Button.Flyout>
          <MenuFlyout>
            <MenuFlyoutItem x:Name="RowFavItem" Text="收藏 / 取消收藏"/>
            <MenuFlyoutItem x:Name="RowRenameItem" Text="重命名"/>
            <MenuFlyoutItem x:Name="RowUnbindItem" Text="解除绑定"/>
            <MenuFlyoutItem x:Name="RowWolItem" Text="远程唤醒 (WOL)" Visibility="Collapsed"/>
          </MenuFlyout>
        </Button.Flyout>
      </Button>
    </StackPanel>
  </Grid>
</Border>"##;

/// 最近/收藏连接行模板。内部命名控件：
/// `RowName`/`RowCode`/`RowWhen`/`RowConnect`/`RowDelete`。
pub const RECENT_ROW_XAML_STR: &str = r##"<Border
    xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
    xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
    Background="{ThemeResource CardBackgroundBrush}"
    BorderBrush="{ThemeResource CardBorderBrush}" BorderThickness="1"
    CornerRadius="10" Padding="14,10">
  <Grid ColumnSpacing="12">
    <Grid.ColumnDefinitions>
      <ColumnDefinition Width="Auto"/>
      <ColumnDefinition Width="*"/>
      <ColumnDefinition Width="Auto"/>
    </Grid.ColumnDefinitions>
    <Border Width="32" Height="32" CornerRadius="9" Background="{ThemeResource AccentSoftBrush}">
      <FontIcon FontFamily="Segoe MDL2 Assets" Glyph="&#xE7F4;" FontSize="14" Foreground="{ThemeResource AccentStrongBrush}"
                HorizontalAlignment="Center" VerticalAlignment="Center"/>
    </Border>
    <StackPanel Grid.Column="1" Spacing="2" VerticalAlignment="Center">
      <TextBlock x:Name="RowName" FontSize="13" FontWeight="SemiBold" Foreground="{ThemeResource TextPrimaryBrush}" MaxWidth="360" TextTrimming="CharacterEllipsis"/>
      <StackPanel Orientation="Horizontal" Spacing="10">
        <TextBlock x:Name="RowCode" FontFamily="Consolas" FontSize="12" Foreground="{ThemeResource TextTertiaryBrush}"/>
        <TextBlock x:Name="RowWhen" FontSize="12" Foreground="{ThemeResource TextTertiaryBrush}"/>
      </StackPanel>
    </StackPanel>
    <StackPanel Grid.Column="2" Orientation="Horizontal" Spacing="4" VerticalAlignment="Center">
      <Button x:Name="RowConnect" Content="连接" Style="{StaticResource AccentSoftButton}"/>
      <Button x:Name="RowDelete" Style="{StaticResource LightGhostIconButton}" AutomationProperties.Name="删除记录" ToolTipService.ToolTip="删除记录">
        <FontIcon FontFamily="Segoe MDL2 Assets" Glyph="&#xE74D;" FontSize="14"/>
      </Button>
    </StackPanel>
  </Grid>
</Border>"##;

/// 设置设备行的在线态呈现。
pub fn set_device_online(
    root: &FrameworkElement,
    online: bool,
    lan: bool,
    is_local: bool,
) -> Result<()> {
    let dot: Ellipse = root.FindName("RowDot")?.cast()?;
    let color = if online {
        windows::UI::Color {
            A: 255,
            R: 0x22,
            G: 0xB5,
            B: 0x73,
        }
    } else {
        windows::UI::Color {
            A: 255,
            R: 0xB9,
            G: 0xC4,
            B: 0xD4,
        }
    };
    let brush = windows::UI::Xaml::Media::SolidColorBrush::new()?;
    brush.SetColor(color)?;
    let shape: windows::UI::Xaml::Shapes::Shape = dot.cast()?;
    shape.SetFill(&brush)?;
    let state: TextBlock = root.FindName("RowState")?.cast()?;
    state.SetText(windows::core::HSTRING::from(if online {
        "在线"
    } else {
        "离线"
    }))?;
    let local: windows::UI::Xaml::Controls::Border = root.FindName("RowLocal")?.cast()?;
    let local: UIElement = local.cast()?;
    local.SetVisibility(if is_local {
        Visibility::Visible
    } else {
        Visibility::Collapsed
    })?;
    let lan_badge: windows::UI::Xaml::Controls::Border = root.FindName("RowLan")?.cast()?;
    let lan_badge: UIElement = lan_badge.cast()?;
    lan_badge.SetVisibility(if lan {
        Visibility::Visible
    } else {
        Visibility::Collapsed
    })?;
    Ok(())
}

// ===== 会话窗口（深色）的列表行 =====

/// 文件传输行模板。内部命名控件：
/// `RowBadge`/`RowIcon`/`RowName`/`RowBar`/`RowStatus`/`RowCancel`。
const TRANSFER_ROW_XAML: &str = r##"<Border
    xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
    xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
    Background="{ThemeResource DarkCardBrush}"
    BorderBrush="{ThemeResource DarkStrokeBrush}" BorderThickness="1"
    CornerRadius="8" Padding="12,9">
  <Grid ColumnSpacing="12">
    <Grid.ColumnDefinitions>
      <ColumnDefinition Width="Auto"/>
      <ColumnDefinition Width="*"/>
      <ColumnDefinition Width="Auto"/>
    </Grid.ColumnDefinitions>
    <Border x:Name="RowBadge" Width="30" Height="30" CornerRadius="8" Background="{ThemeResource DarkAccentTintBrush}" VerticalAlignment="Center">
      <FontIcon x:Name="RowIcon" FontFamily="Segoe MDL2 Assets" FontSize="14" Foreground="{ThemeResource DarkAccentBrush}"
                HorizontalAlignment="Center" VerticalAlignment="Center"/>
    </Border>
    <StackPanel Grid.Column="1" Spacing="4" VerticalAlignment="Center">
      <TextBlock x:Name="RowName" FontSize="13" Foreground="{ThemeResource DarkTextBrush}" TextTrimming="CharacterEllipsis"/>
      <ProgressBar x:Name="RowBar" Height="3" Minimum="0" Maximum="100" Visibility="Collapsed"/>
      <TextBlock x:Name="RowStatus" FontSize="12" Foreground="{ThemeResource DarkText3Brush}" TextTrimming="CharacterEllipsis"/>
    </StackPanel>
    <Button x:Name="RowCancel" Grid.Column="2" Style="{StaticResource GhostSmallButton}" Visibility="Collapsed" VerticalAlignment="Center"
            AutomationProperties.Name="取消传输" ToolTipService.ToolTip="取消传输">
      <FontIcon FontFamily="Segoe MDL2 Assets" Glyph="&#xE711;" FontSize="14"/>
    </Button>
  </Grid>
</Border>"##;

/// 端口映射行模板。内部命名控件：
/// `RowIcon`/`RowName`/`RowState`/`RowStateBadge`/`RowStatus`/`RowToggle`/`RowToggleIcon`/`RowDelete`。
const PORTMAP_ROW_XAML: &str = r##"<Border
    xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
    xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
    Background="{ThemeResource DarkCardBrush}"
    BorderBrush="{ThemeResource DarkStrokeBrush}" BorderThickness="1"
    CornerRadius="8" Padding="12,9">
  <Grid ColumnSpacing="12">
    <Grid.ColumnDefinitions>
      <ColumnDefinition Width="Auto"/>
      <ColumnDefinition Width="*"/>
      <ColumnDefinition Width="Auto"/>
    </Grid.ColumnDefinitions>
    <Border Width="30" Height="30" CornerRadius="8" Background="{ThemeResource DarkRaisedBrush}" VerticalAlignment="Center">
      <FontIcon x:Name="RowIcon" FontFamily="Segoe MDL2 Assets" Glyph="&#xE8AC;" FontSize="14" Foreground="{ThemeResource DarkText2Brush}"
                HorizontalAlignment="Center" VerticalAlignment="Center"/>
    </Border>
    <StackPanel Grid.Column="1" Spacing="3" VerticalAlignment="Center">
      <TextBlock x:Name="RowName" FontSize="13" Foreground="{ThemeResource DarkTextBrush}" TextTrimming="CharacterEllipsis"/>
      <StackPanel Orientation="Horizontal" Spacing="8">
        <Border x:Name="RowStateBadge" CornerRadius="4" Padding="6,1" Background="#223FB950" VerticalAlignment="Center">
          <TextBlock x:Name="RowState" FontSize="11" Foreground="{ThemeResource DarkOnlineBrush}"/>
        </Border>
        <TextBlock x:Name="RowStatus" FontSize="12" Foreground="{ThemeResource DarkText3Brush}" TextTrimming="CharacterEllipsis" MaxWidth="300"/>
      </StackPanel>
    </StackPanel>
    <StackPanel Grid.Column="2" Orientation="Horizontal" Spacing="2" VerticalAlignment="Center">
      <Button x:Name="RowToggle" Style="{StaticResource GhostSmallButton}" AutomationProperties.Name="暂停或启用">
        <FontIcon x:Name="RowToggleIcon" FontFamily="Segoe MDL2 Assets" FontSize="14"/>
      </Button>
      <Button x:Name="RowDelete" Style="{StaticResource GhostSmallButton}" AutomationProperties.Name="删除映射" ToolTipService.ToolTip="删除">
        <FontIcon FontFamily="Segoe MDL2 Assets" Glyph="&#xE74D;" FontSize="14"/>
      </Button>
    </StackPanel>
  </Grid>
</Border>"##;

/// 空列表的引导：一行标题 + 一行说明。内部命名控件：`RowTitle`/`RowCaption`。
const EMPTY_HINT_XAML: &str = r##"<StackPanel
    xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
    xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
    HorizontalAlignment="Center" Margin="0,28,0,0" Spacing="4">
  <TextBlock x:Name="RowTitle" FontSize="13" Foreground="{ThemeResource DarkText2Brush}" HorizontalAlignment="Center"/>
  <TextBlock x:Name="RowCaption" FontSize="12" Foreground="{ThemeResource DarkText3Brush}" HorizontalAlignment="Center" TextAlignment="Center" TextWrapping="Wrap" MaxWidth="280"/>
</StackPanel>"##;

/// 传输行的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferRowState {
    /// 传输中（显示进度条，可以取消）。
    Active,
    /// 已完成。
    Done,
    /// 失败（徽章与说明变红）。
    Failed,
    /// 已取消。
    Cancelled,
}

/// 装配一条文件传输：返回行根元素，以及取消按钮（只在传输中才有）。
pub fn transfer_row(
    upload: bool,
    name: &str,
    status: &str,
    percent: f64,
    state: TransferRowState,
) -> Result<(FrameworkElement, Option<ButtonBase>)> {
    let root: FrameworkElement = XamlReader::Load(TRANSFER_ROW_XAML)?.cast()?;
    named::<TextBlock>(&root, "RowName")?.SetText(HSTRING::from(name))?;
    let status_text: TextBlock = named(&root, "RowStatus")?;
    status_text.SetText(HSTRING::from(status))?;
    set_glyph(
        &root,
        "RowIcon",
        if upload { "\u{E898}" } else { "\u{E896}" },
    )?;
    let bar: ProgressBar = named(&root, "RowBar")?;
    bar.cast::<RangeBase>()?.SetValue(percent)?;
    show(&root, "RowBar", state == TransferRowState::Active)?;
    match state {
        TransferRowState::Failed => {
            paint_background(&root, "RowBadge", "#2EFF5D5D")?;
            paint_foreground(&root, "RowIcon", "#FF6B6B")?;
            status_text.SetForeground(&solid_brush("#FF6B6B")?)?;
        }
        TransferRowState::Done => status_text.SetForeground(&solid_brush("#3FB950")?)?,
        TransferRowState::Active | TransferRowState::Cancelled => {}
    }
    let active = state == TransferRowState::Active;
    show(&root, "RowCancel", active)?;
    let cancel = if active {
        Some(named::<ButtonBase>(&root, "RowCancel")?)
    } else {
        None
    };
    Ok((root, cancel))
}

/// 端口映射规则的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortRowState {
    /// 运行中。
    Running,
    /// 出错（状态标变红）。
    Failed,
    /// 已暂停。
    Paused,
}

/// 装配一条端口映射规则：返回行根元素、暂停 / 启用按钮、删除按钮。
pub fn portmap_row(
    name: &str,
    detail: &str,
    state: PortRowState,
) -> Result<(FrameworkElement, ButtonBase, ButtonBase)> {
    let root: FrameworkElement = XamlReader::Load(PORTMAP_ROW_XAML)?.cast()?;
    named::<TextBlock>(&root, "RowName")?.SetText(HSTRING::from(name))?;
    named::<TextBlock>(&root, "RowStatus")?.SetText(HSTRING::from(detail))?;
    let (label, color, background) = match state {
        PortRowState::Running => ("运行中", "#3FB950", "#223FB950"),
        PortRowState::Failed => ("错误", "#FF6B6B", "#2EFF5D5D"),
        PortRowState::Paused => ("已暂停", "#A3AFC2", "#16FFFFFF"),
    };
    let state_text: TextBlock = named(&root, "RowState")?;
    state_text.SetText(HSTRING::from(label))?;
    state_text.SetForeground(&solid_brush(color)?)?;
    paint_background(&root, "RowStateBadge", background)?;
    // 暂停 / 启用是同一个按钮：图标和提示随状态换
    let paused = state == PortRowState::Paused;
    set_glyph(
        &root,
        "RowToggleIcon",
        if paused { "\u{E768}" } else { "\u{E769}" },
    )?;
    let toggle: ButtonBase = named(&root, "RowToggle")?;
    ToolTipService::SetToolTip(
        &toggle.clone().cast::<UIElement>()?,
        windows::Foundation::PropertyValue::CreateString(HSTRING::from(if paused {
            "启用"
        } else {
            "暂停"
        }))?,
    )?;
    let delete: ButtonBase = named(&root, "RowDelete")?;
    Ok((root, toggle, delete))
}

/// 空列表的引导。
pub fn empty_hint(title: &str, caption: &str) -> Result<FrameworkElement> {
    let root: FrameworkElement = XamlReader::Load(EMPTY_HINT_XAML)?.cast()?;
    named::<TextBlock>(&root, "RowTitle")?.SetText(HSTRING::from(title))?;
    named::<TextBlock>(&root, "RowCaption")?.SetText(HSTRING::from(caption))?;
    Ok(root)
}

/// 按名字取行内的控件。
fn named<T: Interface>(root: &FrameworkElement, name: &str) -> Result<T> {
    root.FindName(name)?.cast()
}

/// 改行内 FontIcon 的码点。
fn set_glyph(root: &FrameworkElement, name: &str, glyph: &str) -> Result<()> {
    let icon: FontIcon = named(root, name)?;
    icon.SetGlyph(HSTRING::from(glyph))
}

/// 显 / 隐行内控件。
fn show(root: &FrameworkElement, name: &str, visible: bool) -> Result<()> {
    let element: UIElement = named(root, name)?;
    element.SetVisibility(if visible {
        Visibility::Visible
    } else {
        Visibility::Collapsed
    })
}

/// `#RRGGBB`（可带两位透明度前缀）→ 纯色画刷。
pub(crate) fn solid_brush(hex: &str) -> Result<Brush> {
    let argb = u32::from_str_radix(hex.trim_start_matches('#'), 16).map_err(|_| {
        windows::core::Error::new(
            windows::Win32::Foundation::E_INVALIDARG,
            HSTRING::from(format!("颜色格式不对：{hex}")),
        )
    })?;
    let brush = SolidColorBrush::new()?;
    brush.SetColor(windows::UI::Color {
        A: if hex.trim_start_matches('#').len() == 8 {
            (argb >> 24) as u8
        } else {
            255
        },
        R: (argb >> 16) as u8,
        G: (argb >> 8) as u8,
        B: argb as u8,
    })?;
    brush.cast()
}

fn paint_background(root: &FrameworkElement, name: &str, hex: &str) -> Result<()> {
    named::<windows::UI::Xaml::Controls::Border>(root, name)?.SetBackground(&solid_brush(hex)?)
}

fn paint_foreground(root: &FrameworkElement, name: &str, hex: &str) -> Result<()> {
    // Foreground 在 IconElement 上（0.37 投影不把基类方法挂到 FontIcon 上）
    named::<FontIcon>(root, name)?
        .cast::<IconElement>()?
        .SetForeground(&solid_brush(hex)?)
}
