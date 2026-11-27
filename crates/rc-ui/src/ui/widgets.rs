//! 可复用 UI 构件（XAML 模板 + FindName 装配）。
//!
//! 模板字符串集中在本模块，`XamlReader::Load` 解析后按名字填充内容并
//! 返回根元素；调用方再以 `FindName` 取出内部控件接线。避免手写
//! `createElement` 链，也避免整页 XAML 的重复。

use windows::core::{Interface, Result};
use windows::UI::Xaml::{
    Controls::{MenuFlyoutItem, TextBlock},
    FrameworkElement,
    Markup::XamlReader,
    Shapes::Ellipse,
    UIElement, Visibility,
};

/// 装配一个设备行，返回行根元素；内容由调用方 `FindName` 填充。
pub fn load_device_row() -> Result<FrameworkElement> {
    XamlReader::Load(DEVICE_ROW_XAML_STR)?.cast()
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
    CornerRadius="10" Padding="16,12">
  <Grid>
    <Grid.ColumnDefinitions>
      <ColumnDefinition Width="Auto"/>
      <ColumnDefinition Width="*"/>
      <ColumnDefinition Width="Auto"/>
    </Grid.ColumnDefinitions>
    <Border Width="40" Height="40" CornerRadius="8" Background="#EDF2FF">
      <FontIcon FontFamily="Segoe MDL2 Assets" Glyph="&#xE7F4;" FontSize="18" Foreground="{ThemeResource PrimaryBrush}"/>
    </Border>
    <StackPanel Grid.Column="1" Margin="14,0,8,0" Spacing="3" VerticalAlignment="Center">
      <StackPanel Orientation="Horizontal" Spacing="8">
        <TextBlock x:Name="RowName" Style="{StaticResource BodyText}" FontWeight="SemiBold" MaxWidth="220" TextTrimming="CharacterEllipsis"/>
        <Border x:Name="RowLocal" Background="#EDF2FF" CornerRadius="4" Padding="6,1" Visibility="Collapsed">
          <TextBlock Text="本机" FontSize="11" Foreground="{ThemeResource PrimaryBrush}"/>
        </Border>
        <Border x:Name="RowLan" Background="#E7F8F0" CornerRadius="4" Padding="6,1" Visibility="Collapsed">
          <TextBlock Text="局域网" FontSize="11" Foreground="{ThemeResource SuccessBrush}"/>
        </Border>
      </StackPanel>
      <StackPanel Orientation="Horizontal" Spacing="6">
        <Ellipse x:Name="RowDot" Width="7" Height="7" Fill="{ThemeResource OfflineBrush}" VerticalAlignment="Center"/>
        <TextBlock x:Name="RowCode" FontFamily="Consolas" FontSize="12" Foreground="{ThemeResource TextTertiaryBrush}" VerticalAlignment="Center"/>
        <TextBlock x:Name="RowState" FontSize="12" Foreground="{ThemeResource TextTertiaryBrush}" VerticalAlignment="Center"/>
      </StackPanel>
    </StackPanel>
    <StackPanel Grid.Column="2" Orientation="Horizontal" Spacing="8" VerticalAlignment="Center">
      <Button x:Name="RowConnect" Content="连接" Style="{StaticResource PrimaryButton}" Height="34"/>
      <Button x:Name="RowMore" Content="&#xE712;" Style="{StaticResource CaptionButton}" FontSize="12" AutomationProperties.Name="更多操作">
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
    Background="{ThemeResource PageBackgroundBrush}"
    BorderBrush="{ThemeResource CardBorderBrush}" BorderThickness="1"
    CornerRadius="8" Padding="14,10">
  <Grid>
    <Grid.ColumnDefinitions>
      <ColumnDefinition Width="Auto"/>
      <ColumnDefinition Width="*"/>
      <ColumnDefinition Width="Auto"/>
    </Grid.ColumnDefinitions>
    <FontIcon FontFamily="Segoe MDL2 Assets" Glyph="&#xE7F4;" FontSize="16" Foreground="{ThemeResource TextTertiaryBrush}" VerticalAlignment="Center"/>
    <StackPanel Grid.Column="1" Margin="12,0" Spacing="2">
      <TextBlock x:Name="RowName" Style="{StaticResource BodyText}" MaxWidth="360" TextTrimming="CharacterEllipsis"/>
      <StackPanel Orientation="Horizontal" Spacing="10">
        <TextBlock x:Name="RowCode" FontFamily="Consolas" FontSize="12" Foreground="{ThemeResource TextTertiaryBrush}"/>
        <TextBlock x:Name="RowWhen" FontSize="12" Foreground="{ThemeResource TextTertiaryBrush}"/>
      </StackPanel>
    </StackPanel>
    <StackPanel Grid.Column="2" Orientation="Horizontal" Spacing="8" VerticalAlignment="Center">
      <Button x:Name="RowConnect" Content="连接" Style="{StaticResource SecondaryButton}" Height="32"/>
      <Button x:Name="RowDelete" Content="&#xE74D;" Style="{StaticResource CaptionButton}" AutomationProperties.Name="删除记录"/>
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
