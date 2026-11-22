import re


# Flutter 列表行的 content-desc 是「歌名 / 时长」（换行已被替换为 ' / '）
SONG_PATTERN = re.compile(r".+ / \d{2}:\d{2}$")


class RoomPage:
    """语聊房页面（进房、上麦、开麦由 conftest 的 setup 保证）。"""

    # 房间底部 More 九宫格图标，没有任何文字，只能用坐标
    MORE_ICON_XY = (295, 1527)
    MORE_PANEL_MARK = "Interaction Tools"

    def __init__(self, ui):
        self.ui = ui

    def ensure_no_overlay(self):
        # 语音调试面板带全屏遮罩，挡住时读不到下面的控件，先关掉
        if self.ui.debug_overlay_up():
            self.ui.dismiss_debug_overlay()

    def open_more(self):
        self.ensure_no_overlay()
        self.ui.tap_xy(*self.MORE_ICON_XY)
        self.ui.wait_text(self.MORE_PANEL_MARK)
        return MorePanel(self.ui)


class MorePanel:
    """房间 More 面板。"""

    MUSIC = "Music"

    def __init__(self, ui):
        self.ui = ui

    def open_music(self):
        self.ui.tap(self.MUSIC, exact=True)
        self.ui.wait_text(MusicPage.ADD_MUSIC[0])
        return MusicPage(self.ui)


class MusicPage:
    """房间 Music 列表页。"""

    ADD_MUSIC = ("Add Music", (360, 1532))
    NO_MUSIC = "No music"
    TITLE_PATTERN = r"^Music\(\d+/\d+\)$"
    ADDED_TOAST = "Music added successfully"

    def __init__(self, ui):
        self.ui = ui

    def title(self):
        return self.ui.read(self.TITLE_PATTERN)

    def is_empty(self):
        return self.ui.has(self.NO_MUSIC, exact=True)

    def has_song(self, name):
        return any(label.startswith(name + " / ") for label in self.ui.labels())

    def open_add_music(self):
        text, xy = self.ADD_MUSIC
        self.ui.tap(text, xy=xy)
        self.ui.wait_text(LocalLibraryPage.TITLE)
        return LocalLibraryPage(self.ui)


class LocalLibraryPage:
    """Local Music Library 选择页。列表顺序每次不同，只能按歌名定位。"""

    TITLE = "Local Music Library"
    TITLE_PATTERN = r"^Local Music Library\(\d+\)$"
    ADD_BUTTON_PATTERN = r"^Add\(\d+\)$"
    SELECT_ALL = "Select all"
    # 轨迹里点中的是行左侧的选择框；行位置变了时 ui.tap 会自动退回元素中心
    SONG_CHECKBOX_XY = (72, 1374)

    def __init__(self, ui):
        self.ui = ui

    def is_open(self):
        return self.ui.has(self.TITLE)

    def total_count(self):
        """标题 Local Music Library(N) 里的歌曲总数，读不到返回 None。"""
        text = self.ui.read(self.TITLE_PATTERN)
        if text is None:
            return None
        return int(re.search(r"\d+", text).group())

    def visible_songs(self):
        return [label for label in self.ui.labels() if SONG_PATTERN.match(label)]

    def is_song_visible(self, name):
        return any(song.startswith(name + " / ") for song in self.visible_songs())

    def add_button_count(self):
        text = self.ui.read(self.ADD_BUTTON_PATTERN)
        if text is None:
            return None
        return int(re.search(r"\d+", text).group())

    def swipe_up(self):
        self.ui.swipe("up")
        self.ui.wait_text(self.TITLE)

    def swipe_down(self):
        self.ui.swipe("down")
        self.ui.wait_text(self.TITLE)

    def scroll_back_to(self, name, max_swipes=12):
        """列表顺序随机，目标可能已经被滑过去（在屏幕上方），向下滑回来直到它出现。"""
        for _ in range(max_swipes):
            if self.is_song_visible(name):
                return True
            self.swipe_down()
        return self.is_song_visible(name)

    def select_song(self, name):
        # 调用前目标已在屏幕内；scroll=True 仅作为兜底（目标在下方时继续上滑）
        self.ui.tap(name + " / ", xy=self.SONG_CHECKBOX_XY, exact=False, scroll=True)
        self.ui.wait_text("Add(1)", exact=True)

    def confirm_add(self, count):
        self.ui.tap(f"Add({count})", exact=True)
        self.ui.wait_text(f"Music({count}/", exact=False)
        return MusicPage(self.ui)
