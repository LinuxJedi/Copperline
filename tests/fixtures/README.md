# Serialized board fixture

`copperhf-board-v79.bin` is bincode 1.x serialization of
`BoardDevice::Copperhf(CopperhfBoard::new())`, using the explicit kind ID 13.
It contains an empty board, no ROM, disk image, path, or user data.

The same bytes are decoded and reproduced by
`zorro_device::state::tests::board_fixture_is_independent_of_optional_features`
in default, core-only and MHI-only CI builds. Keep the fixture stable across
feature selections. It pins the in-process snapshot encoding (`bincode`);
state files carry boards in the `ZORR` chunk as MessagePack, through the same
custom `BoardDevice` impl. If the board payload changes, regenerate the fixture
under a new name that records why.
