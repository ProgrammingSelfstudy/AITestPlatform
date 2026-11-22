"""TC-06c Page Objects：超大小文件（56MB）在 Local Music Library 中选中时被拦截。

Hafa 是 Flutter 应用，只能按 content-desc 定位，坐标只做兜底。
Local Music Library 每次运行的顺序都不同，所以按歌名或时长定位，并且 scroll=True。
"""


class RoomPage:
    """语聊房主界面（已开房、已上麦，由 conftest setup 保证）。"""

    # 房间底部的 More 九宫格图标：没有任何文字，只能按固定坐标点击
    MORE_ICON_XY = (295, 1527)
    # 点开 More 后，面板里稳定出现的文字
    MORE_PANEL_MARK = "Room Tools"
    MUSIC = ("Music", None)
    # 点开 Music 后出现的文字
    MUSIC_PANEL_MARK = "Add Music"

    def __init__(self, ui):
        self.ui = ui

    def open_more(self):
        # 底部 More 九宫格（无文字图标）
        self.ui.tap_xy(*self.MORE_ICON_XY)
        self.ui.wait_text(self.MORE_PANEL_MARK)

    def open_music(self):
        text, xy = self.MUSIC
        self.ui.tap(text, xy=xy)
        self.ui.wait_text(self.MUSIC_PANEL_MARK)
        return MusicPage(self.ui)


class MusicPage:
    """房间音乐面板：Music(x/50)。"""

    ADD_MUSIC = ("Add Music", (360, 1532))
    LIBRARY_MARK = "Local Music Library"

    def __init__(self, ui):
        self.ui = ui

    def open_add_music(self):
        text, xy = self.ADD_MUSIC
        self.ui.tap(text, xy=xy)
        self.ui.wait_text(self.LIBRARY_MARK)
        return LocalLibraryPage(self.ui)


class LocalLibraryPage:
    """Local Music Library 选择页。"""

    TITLE_PREFIX = "Local Music Library"
    ADD_BUTTON_ZERO = "Add(0)"
    SIZE_LIMIT_TOAST = "Music size cannot exceed 50MB"

    def __init__(self, ui):
        self.ui = ui

    def scroll_to_song(self, key, *, exact=False):
        """上滑直到包含 key 的行完整可见，返回元素（找不到返回 None）。"""
        return self.ui.scroll_to(key, exact=exact)

    def read_row(self, pattern):
        """读取第一条匹配正则的行文字，例如 '*** / 05:30'。"""
        return self.ui.read(pattern)

    def tap_song(self, key, *, exact=False):
        """按歌名或时长点击某一行。Toast 停留时间短，点击后不额外等待，由调用方立即检查。"""
        self.ui.tap(key, exact=exact, scroll=True, wait=0)

    def wait_size_toast(self, timeout=4):
        """等待超大小 Toast 出现。出现返回 True，超时返回 False。"""
        try:
            self.ui.wait_text(self.SIZE_LIMIT_TOAST, timeout=timeout)
            return True
        except AssertionError:
            return False

    def wait_toast_gone(self, timeout=10):
        self.ui.wait_gone(self.SIZE_LIMIT_TOAST, timeout=timeout)

    def add_zero_button(self):
        return self.ui.find(self.ADD_BUTTON_ZERO, exact=True)

    def add_zero_button_clickable(self):
        return self.ui.find(self.ADD_BUTTON_ZERO, exact=True, clickable=True)
