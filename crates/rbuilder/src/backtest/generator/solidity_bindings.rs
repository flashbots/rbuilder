use alloy_sol_types::sol;

// PRECOMPILE BINDINGS
sol! {
    function precompile_ecrecover(bytes32 hash, uint8 v, bytes32 r, bytes32 s) public pure returns (address);

    function precompile_sha256(bytes memory input) public view returns (bytes32);

    function precompile_ripemd160(bytes memory input) public view returns (bytes20);

    function precompile_identity(bytes memory input) public view returns (bytes memory);

    function precompile_modExp(uint256 b, uint256 e, uint256 m) public returns (uint256 result);

    function precompile_bn128_add(bytes32 ax, bytes32 ay, bytes32 bx, bytes32 by) public returns (bytes32[2] memory result);

    function precompile_bn128_mul(bytes32 x, bytes32 y, bytes32 scalar) public returns (bytes32[2] memory result);

    function precompile_bn256_pairing(bytes memory input) public returns (bytes32 result);

    function precompile_blake2f(uint32 rounds, bytes32[2] memory h, bytes32[4] memory m, bytes8[2] memory t, bool f) public view returns (bytes32[2] memory);
}

// SLOT CONTENTION BINDINGS
sol! {
    function writeToSlot(uint256 slot, uint256 lowerBoundCurrentExpectedValue, uint256 upperBoundCurrentExpectedValue, uint256 newValue, uint256 payment);

    function writeToSlotProportional(uint256 slot, uint256 lowerBoundCurrentExpectedValue, uint256 upperBoundCurrentExpectedValue, uint256 newValue, uint256 payment);

    function writeToMultipleSlots(uint256[] calldata slotsArray, uint256[] calldata lowerBounds, uint256[] calldata upperBounds, uint256[] calldata newValues, uint256[] calldata payments);

    function writeToMultipleSlotsProprtional(uint256[] calldata slotsArray, uint256[] calldata lowerBounds, uint256[] calldata upperBounds, uint256[] calldata newValues, uint256[] calldata payments);

    function read_coinbase_write_slot(uint256 slot, uint256 topOfBlockValue, uint256 expectedCoinbaseValue, uint256 lowerBoundCurrentExpectedValue, uint256 upperBoundCurrentExpectedValue, uint256 newValue, uint256 payment);

    function readModifyWrite(uint256 slot, uint256 modifierValue);
}
