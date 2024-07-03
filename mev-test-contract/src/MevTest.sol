// SPDX-License-Identifier: Unlicense
pragma solidity ^0.8.13;

contract MevTest {

    /// Sends all value to coinbase.
    function sendToCoinbase() public payable {
        block.coinbase.transfer(msg.value);
    }

    /// Sends all value to the given address.
    function sendTo(address payable to) public payable {
        to.transfer(msg.value);
    }

    /// Check if the value in the slot is equal to the old value, if so increment it and send the value to coinbase.
    function incrementValue(uint256 slot, uint256 oldValue) public payable {
        // check old slot value
        uint256 storedValue;
        assembly {
            storedValue := sload(slot)
        }
        require(storedValue == oldValue, "Old value does not match");
        uint256 newValue = oldValue + 1;
        assembly {
            sstore(slot, newValue)
        }

        if (msg.value > 0) {
            block.coinbase.transfer(msg.value);
        }
    }

    
    /// Just reverts!
    function revert() public payable {
        revert();
    }
}
