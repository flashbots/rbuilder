// SPDX-License-Identifier: Unlicense
pragma solidity ^0.8.13;

import "forge-std/Test.sol";

import "src/MevTest.sol";

contract TestContract is Test {
    MevTest c;

    function setUp() public {
        c = new MevTest();
    }

    function testSendToCoinbase() public {
        uint balanceBefore = block.coinbase.balance;
        c.sendToCoinbase{value: 123}();
        uint balanceAfter = block.coinbase.balance;
        assertEq(balanceAfter - balanceBefore, 123);
    }

    function testTo() public {
        address payable a = payable(vm.addr(1));
        uint balanceBefore = a.balance;
        c.sendTo{value: 123}(a);
        uint balanceAfter = a.balance;
        assertEq(balanceAfter - balanceBefore, 123);
    }

    function testIncrementValue() public {
        // slot value is 0 by default
        c.incrementValue(1, 0);
        c.incrementValue(1, 1);
        c.incrementValue(1, 2);

        // conflicting tx was committed
        vm.expectRevert();
        c.incrementValue(1, 2);

        // different slots don't conflict
        c.incrementValue(2, 0);

        // sends to coinbase
        uint balanceBefore = block.coinbase.balance;
        c.incrementValue{value: 123}(3, 0);
        uint balanceAfter = block.coinbase.balance;
        assertEq(balanceAfter - balanceBefore, 123);
    }
}
