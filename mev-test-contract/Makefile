.PHONY: init test build update printSelectors

init:
	git submodule update --init --recursive


test: init
	forge test

build: init
	forge build

update: build
	cat out/MevTest.sol/MevTest.json | jq '{ "MevTest": .deployedBytecode.object, "MevTestInitBytecode": .bytecode.object }' > ../crates/rbuilder/src/building/testing/contracts.json

printSelectors: build
	forge in MevTest methodIdentifiers
