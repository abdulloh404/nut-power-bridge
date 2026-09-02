#include <linux/err.h>
#include <linux/kernel.h>
#include <linux/kobject.h>
#include <linux/limits.h>
#include <linux/math64.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/power_supply.h>
#include <linux/string.h>
#include <linux/sysfs.h>

#define NUT_UPDATE_MAX_SIZE 128
#define NUT_ENERGY_FULL_UWH 100000000
#define NUT_SECONDS_PER_HOUR 3600

struct nut_power_snapshot {
	int capacity;
	int voltage_now_uv;
	int time_to_empty_sec;
	int status;
	int ac_online;
};

struct nut_power_state {
	struct mutex snapshot_lock;
	struct mutex registration_lock;
	struct nut_power_snapshot snapshot;
	struct power_supply *battery;
	struct power_supply *ac;
};

static struct nut_power_state nut_power;
static struct kobject *nut_power_kobj;

static const enum power_supply_property nut_battery_properties[] = {
	POWER_SUPPLY_PROP_CAPACITY,
	POWER_SUPPLY_PROP_VOLTAGE_NOW,
	POWER_SUPPLY_PROP_TIME_TO_EMPTY_NOW,
	POWER_SUPPLY_PROP_STATUS,
	POWER_SUPPLY_PROP_PRESENT,
	POWER_SUPPLY_PROP_SCOPE,
	POWER_SUPPLY_PROP_ENERGY_NOW,
	POWER_SUPPLY_PROP_ENERGY_FULL,
	POWER_SUPPLY_PROP_ENERGY_FULL_DESIGN,
	POWER_SUPPLY_PROP_POWER_NOW,
};

static const enum power_supply_property nut_ac_properties[] = {
	POWER_SUPPLY_PROP_ONLINE,
	POWER_SUPPLY_PROP_SCOPE,
};

static int nut_energy_now_uwh(const struct nut_power_snapshot *snapshot)
{
	s64 energy_now;

	energy_now = div64_s64((s64)snapshot->capacity * NUT_ENERGY_FULL_UWH,
			       100);

	return clamp_t(s64, energy_now, 0, NUT_ENERGY_FULL_UWH);
}

static int nut_power_now_uw(const struct nut_power_snapshot *snapshot)
{
	s64 power_now;

	if (snapshot->status != POWER_SUPPLY_STATUS_DISCHARGING ||
	    snapshot->time_to_empty_sec <= 0)
		return 0;

	power_now = div64_s64((s64)nut_energy_now_uwh(snapshot) *
			      NUT_SECONDS_PER_HOUR,
			      snapshot->time_to_empty_sec);

	return clamp_t(s64, power_now, 0, INT_MAX);
}

static int nut_battery_get_property(struct power_supply *psy,
				    enum power_supply_property property,
				    union power_supply_propval *value)
{
	struct nut_power_state *state = power_supply_get_drvdata(psy);
	int ret = 0;

	mutex_lock(&state->snapshot_lock);
	switch (property) {
	case POWER_SUPPLY_PROP_CAPACITY:
		value->intval = state->snapshot.capacity;
		break;
	case POWER_SUPPLY_PROP_VOLTAGE_NOW:
		value->intval = state->snapshot.voltage_now_uv;
		break;
	case POWER_SUPPLY_PROP_TIME_TO_EMPTY_NOW:
		value->intval = state->snapshot.time_to_empty_sec;
		break;
	case POWER_SUPPLY_PROP_STATUS:
		value->intval = state->snapshot.status;
		break;
	case POWER_SUPPLY_PROP_PRESENT:
		value->intval = 1;
		break;
	case POWER_SUPPLY_PROP_SCOPE:
		value->intval = POWER_SUPPLY_SCOPE_SYSTEM;
		break;
	case POWER_SUPPLY_PROP_ENERGY_NOW:
		value->intval = nut_energy_now_uwh(&state->snapshot);
		break;
	case POWER_SUPPLY_PROP_ENERGY_FULL:
	case POWER_SUPPLY_PROP_ENERGY_FULL_DESIGN:
		value->intval = NUT_ENERGY_FULL_UWH;
		break;
	case POWER_SUPPLY_PROP_POWER_NOW:
		value->intval = nut_power_now_uw(&state->snapshot);
		break;
	default:
		ret = -EINVAL;
		break;
	}
	mutex_unlock(&state->snapshot_lock);

	return ret;
}

static int nut_ac_get_property(struct power_supply *psy,
			       enum power_supply_property property,
			       union power_supply_propval *value)
{
	struct nut_power_state *state = power_supply_get_drvdata(psy);
	int ret = 0;

	mutex_lock(&state->snapshot_lock);
	if (property == POWER_SUPPLY_PROP_ONLINE)
		value->intval = state->snapshot.ac_online;
	else if (property == POWER_SUPPLY_PROP_SCOPE)
		value->intval = POWER_SUPPLY_SCOPE_SYSTEM;
	else
		ret = -EINVAL;
	mutex_unlock(&state->snapshot_lock);

	return ret;
}

static const struct power_supply_desc nut_battery_desc = {
	.name = "nut-battery",
	.type = POWER_SUPPLY_TYPE_BATTERY,
	.properties = nut_battery_properties,
	.num_properties = ARRAY_SIZE(nut_battery_properties),
	.get_property = nut_battery_get_property,
};

static const struct power_supply_desc nut_ac_desc = {
	.name = "nut-ac",
	.type = POWER_SUPPLY_TYPE_MAINS,
	.properties = nut_ac_properties,
	.num_properties = ARRAY_SIZE(nut_ac_properties),
	.get_property = nut_ac_get_property,
};

static int nut_power_register_supplies(struct nut_power_state *state)
{
	struct power_supply_config config = {
		.drv_data = state,
	};
	struct power_supply *battery;
	struct power_supply *ac;
	int ret;

	battery = power_supply_register(NULL, &nut_battery_desc, &config);
	if (IS_ERR(battery))
		return PTR_ERR(battery);

	ac = power_supply_register(NULL, &nut_ac_desc, &config);
	if (IS_ERR(ac)) {
		ret = PTR_ERR(ac);
		power_supply_unregister(battery);
		return ret;
	}

	state->battery = battery;
	state->ac = ac;

	return 0;
}

static int nut_power_parse_snapshot(char *input,
				    struct nut_power_snapshot *snapshot)
{
	int values[5];
	char *cursor = input;
	char *token;
	size_t field = 0;
	int ret;

	while ((token = strsep(&cursor, " \t\r"))) {
		if (!*token)
			continue;
		if (field == ARRAY_SIZE(values))
			return -EINVAL;

		ret = kstrtoint(token, 10, &values[field]);
		if (ret)
			return ret;
		field++;
	}

	if (field != ARRAY_SIZE(values))
		return -EINVAL;

	snapshot->capacity = values[0];
	snapshot->voltage_now_uv = values[1];
	snapshot->time_to_empty_sec = values[2];
	snapshot->status = values[3];
	snapshot->ac_online = values[4];

	return 0;
}

static ssize_t update_store(struct kobject *kobj,
			    struct kobj_attribute *attribute, const char *buf,
			    size_t count)
{
	struct nut_power_snapshot next;
	struct power_supply *battery;
	struct power_supply *ac;
	char input[NUT_UPDATE_MAX_SIZE];
	int ret;

	if (count < 2 || count >= sizeof(input))
		return -EINVAL;
	if (buf[count - 1] != '\n' || memchr(buf, '\n', count - 1) ||
	    memchr(buf, '\0', count - 1))
		return -EINVAL;

	memcpy(input, buf, count - 1);
	input[count - 1] = '\0';
	ret = nut_power_parse_snapshot(input, &next);
	if (ret)
		return ret;

	if (next.capacity < 0 || next.capacity > 100 ||
	    next.voltage_now_uv < 0 || next.time_to_empty_sec < 0 ||
	    next.status < POWER_SUPPLY_STATUS_UNKNOWN ||
	    next.status > POWER_SUPPLY_STATUS_FULL ||
	    (next.ac_online != 0 && next.ac_online != 1))
		return -ERANGE;

	mutex_lock(&nut_power.registration_lock);
	mutex_lock(&nut_power.snapshot_lock);
	nut_power.snapshot = next;
	mutex_unlock(&nut_power.snapshot_lock);

	battery = nut_power.battery;
	ac = nut_power.ac;
	mutex_unlock(&nut_power.registration_lock);

	power_supply_changed(battery);
	power_supply_changed(ac);

	return count;
}

static struct kobj_attribute update_attribute = __ATTR_WO(update);

static int __init nut_power_init(void)
{
	int ret;

	mutex_init(&nut_power.snapshot_lock);
	mutex_init(&nut_power.registration_lock);
	nut_power.snapshot.capacity = 100;
	nut_power.snapshot.status = POWER_SUPPLY_STATUS_UNKNOWN;
	nut_power.snapshot.ac_online = 1;

	ret = nut_power_register_supplies(&nut_power);
	if (ret)
		return ret;

	nut_power_kobj = kobject_create_and_add("nut_battery", kernel_kobj);
	if (!nut_power_kobj) {
		ret = -ENOMEM;
		goto unregister_supplies;
	}

	ret = sysfs_create_file(nut_power_kobj, &update_attribute.attr);
	if (ret)
		goto put_kobject;

	return 0;

put_kobject:
	kobject_put(nut_power_kobj);
unregister_supplies:
	power_supply_unregister(nut_power.ac);
	power_supply_unregister(nut_power.battery);
	return ret;
}

static void __exit nut_power_exit(void)
{
	struct power_supply *battery;
	struct power_supply *ac;

	sysfs_remove_file(nut_power_kobj, &update_attribute.attr);
	kobject_put(nut_power_kobj);

	mutex_lock(&nut_power.registration_lock);
	battery = nut_power.battery;
	ac = nut_power.ac;
	nut_power.battery = NULL;
	nut_power.ac = NULL;
	mutex_unlock(&nut_power.registration_lock);

	if (ac)
		power_supply_unregister(ac);
	if (battery)
		power_supply_unregister(battery);
}

module_init(nut_power_init);
module_exit(nut_power_exit);

MODULE_AUTHOR("nut-power-bridge contributors");
MODULE_DESCRIPTION("NUT-backed virtual battery and AC power supplies");
MODULE_LICENSE("GPL");
