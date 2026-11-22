import pytest

from generated.pages.tc08_pages import (
    MusicPage,
    PlayerPage,
    RoomPage,
    RoomVolumePanel,
)

SONG = "tst_normal_01"
# 房间号每次运行都不同（conftest 新开房），不写死，运行时从房间标题读取
DEFAULT_VOLUME = 50
NEAR_MAX_MIN = 98                # 「接近 100%」的下限
NEAR_MIN_MAX = 2                 # 「接近 0%」的上限
TARGET_LOW, TARGET_HIGH = 70, 90 # 「约 80%」的允许范围


@pytest.mark.case("TC-08")
def test_tc08_room_volume_default_range_persist(ui, account):
    # 前置（已登录、已开房上麦、个人库有 tst_normal_01、未调过 Room volume）由 conftest 保证
    ui.close_popups()
    ui.dismiss_debug_overlay()
    room = RoomPage(ui).wait_loaded()
    room_id = room.room_id()
    assert room_id, "前置未满足：读不到当前语聊房的房间 ID"

    # 1. 播放 tst_normal_01，打开音乐播放器
    player = room.open_music_list().play_song(SONG)

    # 2. 点击播放器顶部左侧喇叭图标，进入 Room volume
    panel = player.open_room_volume()

    # 3. 记录当前音量数值
    default_value = panel.value()
    assert default_value is not None, "预期1未满足：进入 Room volume 后读不到音量数值"
    assert default_value == DEFAULT_VOLUME, (
        f"预期1未满足：首次进入时音量应默认显示 50%，实际为 {default_value}%"
    )

    # 4. 拖到接近 100%
    high = panel.drag_to_max()
    assert high is not None, "预期3未满足：拖到最右后读不到音量数值，面板可能已关闭或页面崩溃"
    assert high != default_value, f"预期3未满足：拖动后数值没有更新，仍为 {high}%"
    assert high >= NEAR_MAX_MIN, f"预期2未满足：滑块拖到最右应接近 100%，实际为 {high}%"
    assert panel.is_open(), "预期3未满足：拖到最右后 Room volume 面板不在了"

    # 5. 拖到接近 0%
    low = panel.drag_to_min()
    assert low is not None, "预期3未满足：拖到最左后读不到音量数值，面板可能已关闭或页面崩溃"
    assert low != high, f"预期3未满足：拖动后数值没有更新，仍为 {low}%"
    assert low <= NEAR_MIN_MAX, f"预期2未满足：滑块拖到最左应接近 0%，实际为 {low}%"
    assert panel.is_open(), "预期3未满足：拖到最左后 Room volume 面板不在了"

    # 6. 再拖到约 80%
    last = panel.drag_to_about_80()
    assert last is not None, "预期3未满足：拖到约 80% 后读不到音量数值，面板可能已关闭或页面崩溃"
    assert last != low, f"预期3未满足：拖动后数值没有更新，仍为 {last}%"
    assert TARGET_LOW <= last <= TARGET_HIGH, (
        f"预期2未满足：滑块应能停在约 80% 的中间位置，实际为 {last}%"
    )
    assert panel.is_open(), "预期3未满足：拖到约 80% 后 Room volume 面板不在了"
    # 注：截图只能拍到松手后的画面，拖动过程中数值是否逐帧变化无法由界面断言

    # 7. 返回播放器，再重新进入 Room volume
    player = panel.back_to_player()
    panel = player.open_room_volume()
    reopened = panel.value()
    assert reopened == last, (
        f"预期4未满足：返回后再次进入，音量应保持 {last}%，实际为 {reopened}%"
    )

    # 8. 结束 App 进程后重新打开，进入同一房间的播放器，再进入 Room volume
    ui.restart_app()
    ui.close_popups()
    room = RoomPage(ui).wait_loaded()
    room_id_after = room.room_id()
    assert room_id_after == room_id, (
        f"步骤8未满足：重启后应回到同一房间 ID:{room_id}，实际为 ID:{room_id_after}"
    )
    room.open_music()
    if ui.has(PlayerPage.CURRENT_PLAYER):
        player = PlayerPage(ui)
    else:
        # 重启后点 Music 若进的是歌曲列表而不是播放器，就点歌打开播放器
        player = MusicPage(ui).play_song(SONG)
    panel = player.open_room_volume()
    after_restart = panel.value()
    assert after_restart == last, (
        f"预期5未满足：重启 App 后音量应仍为 {last}%，实际为 {after_restart}%"
    )
    # 注：这里只验证了同一账号、同一房间；「按账号全局保存」的跨房间场景不在本脚本内
